//! Scenario `usage_windows` (SME-486 B1): the Claude subscription's usage
//! windows on a Claude Code provider's page, its providers-list row and
//! its conversation's context bar, and not on an HTTP provider's; with
//! SME-523's T3, the resets and a usage hold on the browser's clock.

use super::*;

/// Sets every seeded window of `provider` to `utilization`.
async fn set_utilization(pool: &sqlx::PgPool, provider: i64, utilization: f64) {
    sqlx::query("UPDATE provider_usage_windows SET utilization = $2 WHERE provider_id = $1")
        .bind(provider)
        .bind(utilization)
        .execute(pool)
        .await
        .expect("update the windows");
}

/// Publishes a context-usage update for `conversation`, as a finished
/// call does: the indicator fetches again on it.
fn publish_usage_update(conversation: i64) {
    crate::events::publish(
        conversation,
        crate::events::ConversationEvent::ContextUsageUpdate {
            usage: anthropic::TokenUsage { input_tokens: 40_000, output_tokens: 10_000, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
            context_window: 200_000,
        },
    );
}

/// At the page's current width: whether the indicator and the context
/// label overlap, the bar's height with and without the indicator, and
/// how many lines the indicator takes.
async fn measure_bar(page: &chromiumoxide::Page) -> serde_json::Value {
    let measured: serde_json::Value = page
        .evaluate(
            "(() => { const bar = document.querySelector('.context-usage-bar'); \
             const ind = bar.querySelector('.subscription-usage'); const label = bar.querySelector('.context-usage-label'); \
             const a = ind.getBoundingClientRect(); const b = label.getBoundingClientRect(); \
             const overlap = !(a.right <= b.left || b.right <= a.left || a.bottom <= b.top || b.bottom <= a.top); \
             const withIt = bar.getBoundingClientRect().height; \
             ind.style.display = 'none'; const without = bar.getBoundingClientRect().height; ind.style.display = ''; \
             return JSON.stringify({ overlap, with_it: withIt, without, lines: ind.getClientRects().length, \
               inside: a.right <= bar.getBoundingClientRect().right + 0.5, text: ind.innerText }); })()",
        )
        .await
        .expect("measure the context bar")
        .into_value()
        .expect("a string");
    serde_json::from_str(measured.as_str().expect("the string")).expect("json")
}

fn assert_bar_fits(at: &str, m: &serde_json::Value) {
    assert_eq!(m["overlap"], false, "{at}: the indicator overlaps the context label: {m}");
    assert_eq!(m["with_it"], m["without"], "{at}: the indicator changes the bar's height (SME-75): {m}");
    assert_eq!(m["lines"], 1, "{at}: the indicator wraps: {m}");
    assert_eq!(m["inside"], true, "{at}: the indicator runs past the bar: {m}");
}

/// The zone T3 (SME-523) runs its tabs in: no daylight saving, a
/// half-hour offset, and no short name in en-US, so the label is the
/// browser's `GMT+5:30`.
const ZONE: &str = "Asia/Kolkata";
const ZONE_OFFSET_MINUTES: i64 = 330;

/// `at` on `ZONE`'s wall clock, `HH:MM`.
fn zone_clock(at: chrono::DateTime<chrono::Utc>) -> String {
    (at + chrono::Duration::minutes(ZONE_OFFSET_MINUTES)).format("%H:%M").to_string()
}

/// Opens `url` in a new tab whose browser runs in `ZONE` from its first
/// script on: the zone is read once per page load.
async fn zoned_tab(t: &Scenario<'_>, url: String) -> chromiumoxide::Page {
    let page = t.tab("about:blank").await;
    page.execute(chromiumoxide::cdp::browser_protocol::emulation::SetTimezoneOverrideParams::new(ZONE))
        .await
        .expect("set the tab's time zone");
    page.goto(url.as_str()).await.unwrap_or_else(|e| panic!("open {url}: {e}"));
    page
}

/// T3 (SME-523): a held turn's working line, in `ZONE`, on one line at
/// 360 px; and the line goes back to "Working…" when the hold clears.
async fn assert_hold_line(chat: &chromiumoxide::Page, conversation: i64) {
    use crate::events::{ConversationEvent, publish};
    chat.execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(360, 800, 1.0, false))
        .await
        .expect("resize the page");
    let until = chrono::Utc::now() + chrono::Duration::minutes(90);
    publish(conversation, ConversationEvent::TurnState { running: true, elapsed: 0 });
    publish(conversation, ConversationEvent::UsageHold { until: Some(until.timestamp()) });
    let held = format!("Waiting for the Claude 5-hour usage window \u{b7} resumes about {} (in 1 h", zone_clock(until));
    assert!(wait_for_text(chat, &held, Duration::from_secs(10)).await, "the working line doesn't say {held:?}");
    let fits: bool = chat
        .evaluate("(() => { const line = document.querySelector('.turn-working'); return line.scrollWidth <= line.clientWidth + 0.5; })()")
        .await
        .expect("measure the working line")
        .into_value()
        .expect("a bool");
    assert!(fits, "the held working line runs past its box at 360 px");
    publish(conversation, ConversationEvent::UsageHold { until: None });
    assert!(wait_for_text(chat, "Working\u{2026}", Duration::from_secs(10)).await, "the cleared hold's line");
    publish(conversation, ConversationEvent::TurnState { running: false, elapsed: 0 });
    assert!(wait_for_count(chat, ".turn-working", 0, Duration::from_secs(10)).await, "the turn ended");
}

pub(super) async fn scenario_usage_windows(t: &Scenario<'_>) {
    // A binary that doesn't exist: the page's CLI status and model list
    // fail fast, and the usage block doesn't wait on them.
    let provider = db::create_inference_provider(t.pool, &unique_id("usage"), "claude_cli", "/nonexistent/claude", "api_key", "", false, None, true, None)
        .await
        .expect("create a Claude Code provider")
        .id;
    let now = chrono::Utc::now();
    let rows = [
        db::ProviderUsageWindowRow { window_name: "five_hour".into(), utilization: 0.45, status: "allowed".into(), resets_at: Some(now + chrono::Duration::hours(2)), seen_at: now },
        db::ProviderUsageWindowRow { window_name: "seven_day".into(), utilization: 0.32, status: "allowed".into(), resets_at: Some(now + chrono::Duration::hours(84)), seen_at: now },
    ];
    db::record_usage_windows(t.pool, provider, &rows).await.expect("seed the windows");

    // 1. The provider's page, its resets on the browser's clock (T3).
    let page = zoned_tab(t, t.url(&format!("providers/{provider}"))).await;
    let reset = format!("resets {} GMT+5:30", zone_clock(rows[0].resets_at.expect("a reset")));
    for text in ["Subscription usage", "45%", "32%", reset.as_str()] {
        assert!(wait_for_text(&page, text, Duration::from_secs(15)).await, "the provider page doesn't show {text:?}");
    }

    // 2. The providers list's row.
    let list = t.tab(t.url("providers")).await;
    assert!(wait_for_text(&list, "5h 45% \u{b7} 7d 32%", Duration::from_secs(15)).await, "the providers list's row has no usage");

    // 3. A conversation on that provider shows it in its context bar.
    let conversation = t.conversation().await;
    seed_usage(t.pool, conversation.id, 40_000, 10_000).await;
    db::set_conversation_todos(
        t.pool,
        conversation.id,
        &[crate::anthropic::tools::TodoItem { content: "Watch the usage".to_string(), status: crate::anthropic::tools::TodoStatus::InProgress }],
    )
    .await
    .expect("seed a todo list");
    assert!(db::set_conversation_model(t.pool, conversation.id, provider, "claude-sonnet-4-5").await.expect("set the model"), "the conversation exists");
    let chat = zoned_tab(t, t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&chat, conversation.id).await;
    let indicator = ".context-usage-bar .subscription-usage";
    wait_for_element(&chat, indicator, Duration::from_secs(15)).await;
    assert!(wait_for_text(&chat, "5h 45% \u{b7} 7d 32%", Duration::from_secs(10)).await, "the context bar's indicator text");

    // 4. A new reading shows on the next usage update, coloured.
    set_utilization(t.pool, provider, 0.75).await;
    publish_usage_update(conversation.id);
    assert!(wait_for_text(&chat, "5h 75% \u{b7} 7d 75%", Duration::from_secs(10)).await, "the indicator didn't update");
    assert!(wait_for_count(&chat, ".subscription-usage.subscription-usage-warn", 1, Duration::from_secs(5)).await, "75% is a warning");

    // 5. The widest text fits on one line at 320 px, and at 721 px beside
    //    the todo list, without moving the bar.
    set_utilization(t.pool, provider, 1.0).await;
    publish_usage_update(conversation.id);
    assert!(wait_for_text(&chat, "5h 100% \u{b7} 7d 100%", Duration::from_secs(10)).await, "the widest text");
    assert!(wait_for_count(&chat, ".subscription-usage.subscription-usage-high", 1, Duration::from_secs(5)).await, "100% is high");
    for width in [320, 721] {
        chat.execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(width, 800, 1.0, false))
            .await
            .expect("resize the page");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_bar_fits(&format!("{width} px"), &measure_bar(&chat).await);
    }

    // 6. A held turn's working line (SME-523 T3).
    assert_hold_line(&chat, conversation.id).await;

    // 7. A conversation on the mock HTTP provider has no indicator.
    let other = t.conversation().await;
    seed_usage(t.pool, other.id, 40_000, 10_000).await;
    let plain = t.tab(t.url(&format!("conversation/{}", other.id))).await;
    wait_for_live_client(&plain, other.id).await;
    assert!(wait_for_text(&plain, "25% of context", Duration::from_secs(10)).await, "the context bar shows");
    wait_for_resource(&plain, "/usage-windows").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(wait_for_count(&plain, ".subscription-usage", 0, Duration::from_secs(1)).await, "an HTTP provider's conversation shows no indicator");

    let _ = db::delete_inference_provider(t.pool, provider).await;
}
