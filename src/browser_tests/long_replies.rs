use super::*;

/// SME-30: a long reply (about 30 KB, ten code blocks) keeps up while it
/// streams. The whole reply re-renders on every delta, so this bounds how
/// far the page falls behind: the last delta shows within 3 s of being
/// sent, on the debug bundle the tier serves.
pub(super) async fn scenario_markdown_long_reply(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    let section = |i: usize| {
        format!(
            "### Part {i}\n\nSome **text** with `code` and a [link](https://example.com/{i}). {}\n\n- one\n- two\n\n```rust\nfn part_{i}() -> u32 {{\n    let x = {i};\n    x * 2\n}}\n```\n\n",
            "Filler words to make the reply long. ".repeat(60)
        )
    };
    let reply: String = (0..10).map(section).collect::<String>() + "THE-END-MARKER";
    publish(conversation.id, ConversationEvent::ReplyReset {});
    let chunk = reply.len().div_ceil(100);
    let mut offset = 0;
    let mut sent_last = tokio::time::Instant::now();
    while offset < reply.len() {
        let mut end = (offset + chunk).min(reply.len());
        while !reply.is_char_boundary(end) {
            end += 1;
        }
        publish(conversation.id, ConversationEvent::ReplyDelta { text: reply[offset..end].to_string(), offset });
        offset = end;
        sent_last = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        wait_for_text(&page, "THE-END-MARKER", Duration::from_secs(30)).await,
        "the end of a long streamed reply never showed"
    );
    let lag = sent_last.elapsed();
    println!("browser tier: markdown_long_reply: {} bytes in 100 deltas, end shown {lag:?} after the last", reply.len());
    let blocks: i64 = page
        .evaluate("document.querySelectorAll('.message-streaming pre.hl-code').length")
        .await
        .expect("count highlighted blocks")
        .into_value()
        .expect("a number");
    assert_eq!(blocks, 10, "every closed block should be highlighted");
    assert!(lag < Duration::from_secs(3), "the page fell {lag:?} behind a long streamed reply");
}

/// The seconds a "Working…" line's timer shows, or -1 while it's absent:
/// `Working… 12s` reads as 12, `Working… 2m 05s` as 125.
pub(super) async fn working_line_seconds(page: &chromiumoxide::Page) -> i64 {
    page.evaluate(
        r#"(() => { const line = document.querySelector('.turn-working');
             if (!line) return -1;
             const m = line.innerText.match(/(?:(\d+)m )?(\d+)s/);
             return m ? (m[1] ? +m[1] * 60 : 0) + +m[2] : -1; })()"#,
    )
    .await
    .expect("read the working line's timer")
    .into_value()
    .expect("a number")
}

/// `working_line_seconds` polled until the line has been drawn. A tab's
/// first paint of it can lag the point where the client reports itself
/// live, and a -1 read would otherwise be reported as the timer starting
/// over. A line that never appears still reads -1, so the caller's
/// assertion fails with its own message.
pub(super) async fn drawn_working_line_seconds(page: &chromiumoxide::Page) -> i64 {
    let settled = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let shown = working_line_seconds(page).await;
        if shown >= 0 || tokio::time::Instant::now() >= settled {
            return shown;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// SME-160: the "Working…" timer counts from when the turn started, not
/// from when a tab came to be. Both ways the report had: reload mid-turn
/// and the timer comes back at the turn's real age on the server's clock
/// instead of starting over at 0s; open a second tab mid-turn and it
/// shows that same age straight away, rather than counting from its own
/// load. The mock's reply is held at 75 words (15s) so the spell outlives
/// the reload and the second tab both.
pub(super) async fn scenario_working_line_timer_survives_a_reload(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let _long_reply = t.mock.longer_reply(75);
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    send_from(&page, "how long will this take?").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let before = loop {
        let shown = working_line_seconds(&page).await;
        if shown >= 2 || tokio::time::Instant::now() >= deadline {
            break shown;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        before >= 2,
        "the working line should appear and count to at least 2s while the turn runs: {before}s"
    );
    // A second tab opened mid-spell: it has watched nothing, so its
    // number can only come from the server. On the unfixed code it shows
    // about 1s — the age of its own page.
    let other = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&other, conversation.id).await;
    let second_tab = drawn_working_line_seconds(&other).await;
    assert!(
        second_tab >= before,
        "a tab opened mid-turn counted from its own load: {second_tab}s against the {before}s the first tab had shown"
    );
    page.reload().await.expect("reload mid-turn");
    wait_for_live_client(&page, conversation.id).await;
    // The reconnect pull runs before the page marks itself live, so the
    // timer is already the turn's age on the server's clock; the poll is
    // only for the re-render. The 300ms it used to sleep was a bet on a
    // render whose lag nothing had measured, and a lost bet read as the
    // timer having started over.
    let after = drawn_working_line_seconds(&page).await;
    assert!(
        after >= second_tab,
        "the timer started over with the reload: {before}s before it, {second_tab}s in a second tab opened mid-turn, {after}s after the reload"
    );
    // The turn finishes as usual, and the line goes with it: the reply,
    // which only had to outlast the reload, sends its rest at once.
    let _hurried = t.mock.hurry();
    assert!(
        t.mock.wait_until_idle(Duration::from_secs(30)).await,
        "the reply never finished upstream"
    );
    assert!(
        wait_for_count(&page, ".turn-working", 0, Duration::from_secs(15)).await,
        "the working line should go away once the turn has ended"
    );
}

/// Streams a 100-delta reply into `conversation` (open in `page`, live)
/// with a turn running, then lets the working line tick twice. Returns how
/// long after the last delta its end showed, and the page's main-thread
/// script time over the whole stream, in seconds.
pub(super) async fn stream_and_measure(page: &chromiumoxide::Page, conversation: i64, marker: &str) -> (Duration, f64) {
    use crate::events::{ConversationEvent, publish};
    use chromiumoxide::cdp::browser_protocol::performance::{EnableParams, Metric};
    page.execute(EnableParams::default()).await.expect("enable performance metrics");
    let script_seconds =
        |metrics: Vec<Metric>| metrics.iter().find(|m| m.name == "ScriptDuration").map(|m| m.value).unwrap_or(0.0);
    let before = script_seconds(page.metrics().await.expect("read metrics"));
    publish(conversation, ConversationEvent::TurnState { running: true, elapsed: 0 });
    publish(conversation, ConversationEvent::ReplyReset {});
    let reply: String = (0..100).map(|i| format!("word{i} ")).collect::<String>() + marker;
    let chunk = reply.len().div_ceil(100);
    let mut offset = 0;
    let mut sent_last = tokio::time::Instant::now();
    while offset < reply.len() {
        let end = (offset + chunk).min(reply.len());
        publish(conversation, ConversationEvent::ReplyDelta { text: reply[offset..end].to_string(), offset });
        offset = end;
        sent_last = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(wait_for_text(page, marker, Duration::from_secs(60)).await, "the end of the streamed reply never showed");
    let lag = sent_last.elapsed();
    // The working line ticks once a second.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let script = script_seconds(page.metrics().await.expect("read metrics")) - before;
    publish(conversation, ConversationEvent::TurnState { running: false, elapsed: 0 });
    (lag, script)
}

/// SME-57 (c): streaming a reply into a long conversation costs what the
/// reply costs, not what the conversation does. The same 100-delta reply
/// (and the working line's ticks) streams into a one-message conversation
/// and into one with 300 messages (markdown replies with code, tool rows);
/// the page's script time for the long one stays within 3x the short one's,
/// and the messages already shown keep their DOM nodes untouched. Before,
/// every delta and every tick re-rendered and re-parsed the whole
/// transcript, so the long one cost many times more (the page still kept
/// up: the cost is CPU, not lag).
pub(super) async fn scenario_streaming_into_a_long_transcript(t: &Scenario<'_>) {
    let short = t.conversation().await;
    seed_user_message(t.pool, short.id, "One question.").await;
    let page = t.tab(t.url(&format!("conversation/{}", short.id))).await;
    wait_for_live_client(&page, short.id).await;
    let (short_lag, short_script) = stream_and_measure(&page, short.id, "SHORT-END-MARKER").await;

    let long = t.conversation().await;
    for i in 0..100 {
        seed_user_message(t.pool, long.id, &format!("Question {i}: what does part {i} do?")).await;
        let reply = format!(
            "### Part {i}\n\nIt **doubles** `x`. {}\n\n```rust\nfn part_{i}() -> u32 {{\n    let x = {i};\n    x * 2\n}}\n```\n",
            "Some words about it. ".repeat(20)
        );
        db::create_message(t.pool, long.id, "assistant", &[
            anthropic::ContentBlock::Text { text: reply },
            anthropic::ContentBlock::ToolUse {
                id: format!("toolu_long_{i}"),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": format!("/workspace/part_{i}.rs")}),
            },
        ])
        .await
        .expect("seed a reply");
        db::create_message(t.pool, long.id, "user", &[anthropic::ContentBlock::ToolResult {
            tool_use_id: format!("toolu_long_{i}"),
            content: format!("fn part_{i}() {{}}"),
            is_error: None,
        }])
        .await
        .expect("seed a tool result");
    }
    let page = t.tab(t.url(&format!("conversation/{}", long.id))).await;
    wait_for_live_client(&page, long.id).await;
    assert!(
        wait_for_count(&page, ".messages .message-assistant", 100, Duration::from_secs(30)).await,
        "the long conversation never showed"
    );
    // Tag the first reply's node and count every change under it.
    page.evaluate(
        "(() => { const m = document.querySelector('.messages .message-assistant'); m.__smeltTag = 'first'; \
          window.__smeltMutations = 0; \
          new MutationObserver(r => { window.__smeltMutations += r.length; }) \
            .observe(m, { subtree: true, childList: true, characterData: true, attributes: true }); })()",
    )
    .await
    .expect("tag the first reply");
    let (long_lag, long_script) = stream_and_measure(&page, long.id, "LONG-END-MARKER").await;
    let first: serde_json::Value = page
        .evaluate(
            "(() => ({ same: document.querySelector('.messages .message-assistant')?.__smeltTag === 'first', \
                       mutations: window.__smeltMutations }))()",
        )
        .await
        .expect("read the first reply")
        .into_value()
        .expect("facts");
    println!(
        "browser tier: streaming_into_a_long_transcript: script {short_script:.2} s short, {long_script:.2} s long \
         ({:.1}x); end shown {short_lag:?} / {long_lag:?} after the last delta; first reply {first}",
        long_script / short_script.max(0.001)
    );
    assert_eq!(first["same"], true, "the first reply's node was replaced: {first}");
    assert_eq!(first["mutations"], 0, "the first reply changed while another streamed: {first}");
    assert!(
        long_script <= 3.0 * short_script.max(0.05),
        "streaming into 300 messages took {long_script:.2} s of script, against {short_script:.2} s into one"
    );
}

/// SME-57 (d), from SME-58: a conversation in the sidebar opens from the
/// keyboard. Each row's title is a link and a tab stop that Enter or Space
/// opens; the row itself isn't one, so its Delete isn't nested inside the
/// link (a screen reader would read "Delete" as part of the link's name,
/// and some can't reach a button inside a link). Delete stays a tab stop
/// of its own: Enter on it arms it, without opening the conversation. The
/// rows took mouse clicks only.
pub(super) async fn scenario_conversation_rows_by_keyboard(t: &Scenario<'_>) {
    let first = t.conversation().await;
    seed_user_message(t.pool, first.id, "the first row").await;
    let second = t.conversation().await;
    seed_user_message(t.pool, second.id, "the second row").await;
    let page = t.tab(t.url(&format!("conversation/{}", first.id))).await;
    wait_for_live_client(&page, first.id).await;
    let row = |id: i64| format!("[data-conversation-id=\"{id}\"]");
    let title = |id: i64| format!("{} .conversation-title", row(id));
    let roles: serde_json::Value = page
        .evaluate(format!(
            "(() => {{ const r = document.querySelector('{}'), t = r.querySelector('.conversation-title'); \
               return {{ row: r.getAttribute('role') || 'none', title: t.getAttribute('role') || 'none', tab: t.tabIndex }}; }})()",
            row(second.id).replace('\'', "\\'")
        ))
        .await
        .expect("read the row's roles")
        .into_value()
        .expect("facts");
    assert_eq!(roles["title"], "link", "a row's title should be its link: {roles}");
    assert_eq!(roles["row"], "none", "the row around the link and its Delete shouldn't be a link too: {roles}");
    assert!(roles["tab"].as_i64().is_some_and(|t| t >= 0), "the title should be a tab stop: {roles}");
    let at = |id: i64| format!("/conversation/{id}");
    let url_now = |page: &chromiumoxide::Page| {
        let page = page.clone();
        async move { page.url().await.ok().flatten().unwrap_or_default() }
    };
    let wait_for_url = |page: &chromiumoxide::Page, suffix: String| {
        let page = page.clone();
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if page.url().await.ok().flatten().is_some_and(|u| u.ends_with(&suffix)) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }
    };
    let second_row = wait_for_element(&page, &title(second.id), Duration::from_secs(10)).await;
    second_row.focus().await.expect("focus the second row");
    second_row.press_key("Enter").await.expect("press Enter");
    assert!(wait_for_url(&page, at(second.id)).await, "Enter on a row should open it: at {}", url_now(&page).await);
    let first_row = wait_for_element(&page, &title(first.id), Duration::from_secs(10)).await;
    first_row.focus().await.expect("focus the first row");
    first_row.press_key(" ").await.expect("press Space");
    assert!(wait_for_url(&page, at(first.id)).await, "Space on a row should open it: at {}", url_now(&page).await);
    let delete = wait_for_element(&page, &format!("{} .delete-conversation", row(second.id)), Duration::from_secs(10)).await;
    delete.focus().await.expect("focus the second row's Delete");
    delete.press_key("Enter").await.expect("press Enter on Delete");
    wait_for_element(&page, &format!("{} .delete-conversation.confirm", row(second.id)), Duration::from_secs(5)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let url = url_now(&page).await;
    assert!(url.ends_with(&at(first.id)), "Enter on Delete should arm it, not open its row: at {url}");
}

/// SME-57 (d), from SME-58: the message box has a label, not only a
/// placeholder, which disappears as soon as anything is typed.
pub(super) async fn scenario_message_box_label(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_element(&page, CHAT_INPUT, Duration::from_secs(10)).await;
    let label: serde_json::Value = page
        .evaluate(format!(
            "(() => {{ const i = document.querySelector('{}'); \
               return {{ labels: i.labels ? i.labels.length : 0, text: i.labels && i.labels[0] ? i.labels[0].textContent.trim() : null }}; }})()",
            CHAT_INPUT.replace('\'', "\\'")
        ))
        .await
        .expect("read the message box's label")
        .into_value()
        .expect("facts");
    assert_eq!(label["labels"], 1, "the message box should have one label: {label}");
    assert!(label["text"].as_str().is_some_and(|t| !t.is_empty()), "its label should say something: {label}");
}

/// SME-57 (d), from SME-58: an error line on the chat page is announced:
/// it uses `pages::ErrorText` (`role="alert"`) like the other pages, not a
/// bare `p.error`.
pub(super) async fn scenario_chat_errors_are_alerts(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    publish(conversation.id, ConversationEvent::NotificationDeliveryFailed { detail: "NOTICE-ALERT-MARKER".into() });
    publish(conversation.id, ConversationEvent::TurnError { message: "TURN-ALERT-MARKER".into() });
    for marker in ["NOTICE-ALERT-MARKER", "TURN-ALERT-MARKER"] {
        assert!(wait_for_text(&page, marker, Duration::from_secs(10)).await, "{marker} never showed");
        let line: serde_json::Value = page
            .evaluate(format!(
                "(() => {{ const e = [...document.querySelectorAll('.error')].find(e => e.innerText.includes('{marker}')); \
                   return {{ found: !!e, role: e ? (e.getAttribute('role') || 'none') : 'none' }}; }})()"
            ))
            .await
            .expect("read the error line's role")
            .into_value()
            .expect("facts");
        assert_eq!(line["found"], true, "no .error line has {marker}: {line}");
        assert_eq!(line["role"], "alert", "the error line with {marker} should be an alert: {line}");
    }
}

/// SME-57 (d), from SME-58: a message that can't be read shows as an
/// error, in the error colours, not as an ordinary bubble (its class,
/// `message-error`, had no CSS rule).
pub(super) async fn scenario_unreadable_message(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    sqlx::query("INSERT INTO messages (conversation_id, role, content) VALUES ($1, 'assistant', 'not json')")
        .bind(conversation.id)
        .execute(t.pool)
        .await
        .expect("seed an unreadable message");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_element(&page, ".message-error", Duration::from_secs(10)).await;
    let colours: serde_json::Value = page
        .evaluate(
            "(() => { const probe = document.createElement('p'); probe.className = 'error'; document.body.appendChild(probe); \
               const want = getComputedStyle(probe).color; probe.remove(); \
               return { want, got: getComputedStyle(document.querySelector('.message-error')).color }; })()",
        )
        .await
        .expect("read the colours")
        .into_value()
        .expect("colours");
    assert_eq!(colours["got"], colours["want"], "an unreadable message should be in the error colour: {colours}");
}
