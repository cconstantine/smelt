//! SME-482: the chat header's Compact button, end to end in a browser.

use super::*;

/// Boxes as `[left, top, right, bottom]` for each selector (all zeros for
/// one that isn't there), and the viewport's width and height last.
async fn boxes(page: &chromiumoxide::Page, selectors: &[&str]) -> Vec<Vec<f64>> {
    let list = serde_json::to_string(selectors).expect("selectors serialize");
    page.evaluate(format!(
        "(() => {{ const out = {list}.map(s => {{ const e = document.querySelector(s); \
         if (!e) return [0, 0, 0, 0]; const r = e.getBoundingClientRect(); \
         return [r.left, r.top, r.right, r.bottom]; }}); \
         out.push([window.innerWidth, window.innerHeight, 0, 0]); return out; }})()"
    ))
    .await
    .expect("measure the header")
    .into_value()
    .expect("boxes")
}

/// Types `text` into the popover's note box as a user would.
async fn type_note(page: &chromiumoxide::Page, text: &str) {
    let note = wait_for_element(page, ".compact-note", Duration::from_secs(5)).await;
    note.focus().await.expect("focus the note");
    note.type_str(text).await.expect("type the note");
}

/// The meter's text: what the tab shows of the conversation's usage.
async fn meter(page: &chromiumoxide::Page) -> String {
    page.evaluate("document.querySelector('.context-usage-bar')?.innerText ?? ''")
        .await
        .expect("read the meter")
        .into_value()
        .expect("text")
}

/// T19: Compact, a note, Compact now: "Compacting…" while the (slow)
/// summary streams, then the divider "on request", the popover closed and
/// the meter refetched. Then the layout: both header buttons and the
/// popover on screen at 375 px, clear of the composer; and at 721 px with
/// the todo list and sandbox panel open and a 60-character title, neither
/// button clipped.
pub(super) async fn scenario_compact_now(t: &Scenario<'_>) {
    let pool = t.pool;
    let conversation = t.conversation().await;
    seed_usage(pool, conversation.id, 40_000, 10_000).await;
    for (role, text) in [("assistant", "hi there"), ("user", "keep the plan"), ("assistant", "noted")] {
        db::create_message(pool, conversation.id, role, &[anthropic::ContentBlock::Text { text: text.to_string() }])
            .await
            .expect("seed an exchange");
    }
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    assert!(wait_for_text(&page, "25% of context", Duration::from_secs(10)).await, "the seeded usage shows first");

    click_when_present(&page, ".conversation-compact-toggle", Duration::from_secs(5)).await;
    type_note(&page, "the plan").await;
    click_when_present(&page, ".compact-now", Duration::from_secs(5)).await;
    // The mock's reply streams for about 5 s, the summary's included.
    assert!(
        wait_until(
            || async {
                page.evaluate(
                    "(() => { const b = document.querySelector('.conversation-compact-toggle'); \
                     return b.disabled && b.innerText.includes('Compacting'); })()",
                )
                .await
                .ok()
                .and_then(|v| v.into_value::<bool>().ok())
                    == Some(true)
            },
            Duration::from_secs(5)
        )
        .await,
        "the button reads Compacting… and is disabled while the summary runs"
    );
    assert!(
        wait_for_text(&page, "Conversation compacted: on request", Duration::from_secs(20)).await,
        "the divider lands live, naming the request"
    );
    assert!(wait_for_count(&page, ".compact-popover", 0, Duration::from_secs(5)).await, "the popover closes");
    assert!(
        wait_until(|| async { !meter(&page).await.contains("25% of context") }, Duration::from_secs(5)).await,
        "the meter refetched: no stale usage ({})",
        meter(&page).await
    );

    // 375 px: both buttons, and the open popover, on screen, clear of the composer.
    page.execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(375, 700, 2.0, true))
        .await
        .expect("emulate a phone");
    click_when_present(&page, ".conversation-compact-toggle", Duration::from_secs(5)).await;
    wait_for_element(&page, ".compact-popover", Duration::from_secs(5)).await;
    let b = boxes(&page, &[".conversation-thinking-toggle", ".conversation-compact-toggle", ".compact-popover", ".composer"]).await;
    // Against the emulated width itself: content past it widens the
    // layout viewport (`innerWidth`), which would then excuse it.
    let width = 375.0;
    for (i, what) in [(0, "Show thinking"), (1, "Compact"), (2, "the popover")] {
        assert!(b[i][0] >= 0.0 && b[i][2] <= width && b[i][2] > b[i][0], "{what} is on screen at 375 px: {b:?}");
    }
    assert!(b[2][3] <= b[3][1], "the popover doesn't cover the composer: {b:?}");
    let note = wait_for_element(&page, ".compact-note", Duration::from_secs(5)).await;
    note.focus().await.expect("focus the note");
    note.press_key("Escape").await.expect("press Escape");
    assert!(wait_for_count(&page, ".compact-popover", 0, Duration::from_secs(5)).await, "Esc closes it");
    click_when_present(&page, ".conversation-compact-toggle", Duration::from_secs(5)).await;
    click_when_present(&page, ".compact-backdrop", Duration::from_secs(5)).await;
    assert!(wait_for_count(&page, ".compact-popover", 0, Duration::from_secs(5)).await, "a click outside closes it");

    // 721 px, with the todo list and sandbox panel beside the chat and a long title.
    let title = "A sixty-character conversation title for the header's checks";
    assert_eq!(title.chars().count(), 60);
    sqlx::query("UPDATE conversations SET title = $1 WHERE id = $2")
        .bind(title)
        .bind(conversation.id)
        .execute(pool)
        .await
        .expect("a long title");
    db::set_conversation_todos(
        pool,
        conversation.id,
        &[anthropic::tools::TodoItem { content: "a step".to_string(), status: anthropic::tools::TodoStatus::Pending }],
    )
    .await
    .expect("a todo list");
    sandbox::create_pod(pool, conversation.id, Default::default()).await.expect("a pod for the sandbox panel");
    page.execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(721, 900, 1.0, false))
        .await
        .expect("emulate a small laptop");
    page.goto(t.url(&format!("conversation/{}", conversation.id))).await.expect("reload with the panels");
    wait_for_live_client(&page, conversation.id).await;
    for panel in [".todo-panel", ".sandbox-panel"] {
        wait_for_element(&page, panel, Duration::from_secs(30)).await;
    }
    let b = boxes(&page, &[".conversation-header", ".conversation-thinking-toggle", ".conversation-compact-toggle"]).await;
    // Measured: the pod goes before anything below can fail.
    let _ = sandbox::terminate_pod(pool, conversation.id).await;
    for (i, what) in [(1, "Show thinking"), (2, "Compact")] {
        assert!(
            b[i][0] >= b[0][0] && b[i][2] <= b[0][2] + 0.5 && b[i][2] > b[i][0],
            "{what} isn't clipped at 721 px with both panels open: {b:?}"
        );
    }
    assert!(b[2][0] >= b[1][2], "the two buttons don't overlap: {b:?}");
}
