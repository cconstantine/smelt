use super::*;

/// Scenarios 1–4: the sandbox panel's terminals, one flow on one pod. A
/// conversation has at most one live pod now (see SME-11's "One pod per
/// conversation"), so there's no tab bar to click through — both
/// terminals render straight through as soon as the panel loads.
pub(super) async fn scenario_terminals(t: &Scenario<'_>) {
    let pool = t.pool;
    let conversation = t.conversation().await;

    // --- Scenario 1: cold-load panel population, one pod, two terminals
    // in it. ---
    sandbox::create_pod(pool, conversation.id, Default::default()).await.expect("create_pod");
    let terminal_a1 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a1)");
    let terminal_a2 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a2)");

    let page = t.tab(t.url("")).await;
    // Every scenario assumes the page is styled; a layout check on an
    // unstyled page proves nothing (SME-40 F17: the stylesheet was a 404
    // in this tier, so every page ran unstyled).
    // Every part of it (SME-163: the stylesheet is in parts, one per file).
    let stylesheets: Vec<(String, u16)> = page
        .evaluate(
            "Promise.all([...document.querySelectorAll('link[rel=stylesheet]')].map(l => \
             fetch(l.href).then(r => [l.getAttribute('href'), r.status])))",
        )
        .await
        .expect("fetch the stylesheets")
        .into_value()
        .expect("each stylesheet's address and status");
    assert!(stylesheets.len() >= 8, "the app's seven parts and highlight.css, got {stylesheets:?}");
    for (href, status) in &stylesheets {
        assert_eq!(*status, 200, "the page's stylesheet {href} doesn't load");
    }
    // Clicking its sidebar entry, same as a real user, though a direct
    // `/conversation/{id}` URL would work too now that routing exists. By
    // id: another scenario's conversation can sort first.
    click_when_present(
        &page,
        &format!(".conversation-item[data-conversation-id=\"{}\"]", conversation.id),
        Duration::from_secs(10),
    )
    .await;

    for text in [format!("terminal {terminal_a1}"), format!("terminal {terminal_a2}")] {
        assert!(
            wait_for_text(&page, &text, Duration::from_secs(10)).await,
            "cold snapshot should render {text}"
        );
    }

    // --- Scenario 2: live streaming output, no reload ---
    // `anthropic::tools::execute` (unlike sandbox.rs's own lower-level
    // integration test, which calls db::create_terminal_command +
    // sandbox::send_command directly) already creates the
    // terminal_commands row itself — this is the real tool-dispatch
    // entry point, one level up.
    let command_id = unique_id("cmd-1");
    anthropic::tools::execute(
        pool,
        conversation.id,
        &command_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal_a1, "command": "echo hello_from_browser_test"}),
    )
    .await
    .expect("run_terminal_command");
    assert!(
        wait_for_text(&page, "hello_from_browser_test", Duration::from_secs(15)).await,
        "live output should stream into the DOM with no page reload"
    );

    // --- Scenario 3: terminate_terminal removes exactly the right card ---
    sandbox::terminate_terminal(pool, terminal_a2).await.expect("terminate_terminal (a2)");
    assert!(
        wait_for_text_gone(&page, &format!("terminal {terminal_a2}"), Duration::from_secs(10)).await,
        "a terminated terminal's card should disappear"
    );
    assert!(
        page.evaluate("document.body.innerText").await.expect("read body text").into_value::<String>().expect("string")
            .contains(&format!("terminal {terminal_a1}")),
        "a sibling terminal in the same pod should be completely unaffected"
    );
    // A terminal that goes away while the tab is reconnecting: its
    // live event reaches nobody, and the reconnect's snapshot is what
    // must take the card away (SME-43; it used to stay for good).
    // `forget` ends the tab's stream, which reconnects 1.5 s later.
    let terminal_a3 = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal (a3)");
    assert!(
        wait_for_text(&page, &format!("terminal {terminal_a3}"), Duration::from_secs(10)).await,
        "a new terminal's card should appear live"
    );
    crate::events::forget(conversation.id);
    sandbox::terminate_terminal(pool, terminal_a3).await.expect("terminate_terminal (a3)");
    assert!(
        wait_for_text_gone(&page, &format!("terminal {terminal_a3}"), Duration::from_secs(10)).await,
        "a terminal terminated while the tab was reconnecting should be gone once it has"
    );

    // --- Scenario 4: reload mid-command reconstructs state, live updates resume ---
    let long_command_id = unique_id("cmd-2");
    anthropic::tools::execute(
        pool,
        conversation.id,
        &long_command_id,
        "run_terminal_command",
        &serde_json::json!({"terminal_id": terminal_a1, "command": "sleep 3 && echo done_after_reload"}),
    )
    .await
    .expect("run_terminal_command");

    page.goto(t.url("")).await.expect("reload the app");
    click_when_present(
        &page,
        &format!(".conversation-item[data-conversation-id=\"{}\"]", conversation.id),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        wait_for_text(&page, &format!("terminal {terminal_a1}"), Duration::from_secs(10)).await,
        "the fresh page load's snapshot pull should reconstruct the terminal"
    );
    assert!(
        wait_for_text(&page, "done_after_reload", Duration::from_secs(15)).await,
        "live updates should resume after the reload, not just the pre-reload snapshot"
    );

    // Best-effort teardown of what this scenario created.
    let _ = sandbox::terminate_terminal(pool, terminal_a1).await;
    let _ = sandbox::terminate_pod(pool, conversation.id).await;
}

/// Seeds `conversation` with one user message and `input`/`output` tokens of
/// usage, for the context meter.
pub(super) async fn seed_usage(pool: &sqlx::PgPool, conversation: i64, input_tokens: i64, output_tokens: i64) {
    db::create_message(
        pool,
        conversation,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "hello".to_string(),
        }],
    )
    .await
    .expect("seed a message");
    db::upsert_conversation_usage(
        pool,
        conversation,
        &anthropic::TokenUsage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
}

/// Scenario 5: the always-visible context-usage indicator and its
/// click-through detail view — see SME-18. Seeded directly
/// (db::create_message/upsert_conversation_usage) rather than sent through
/// the model: this tier verifies the DOM, and a real Anthropic call needs
/// credentials this test environment (and CI) doesn't have.
pub(super) async fn scenario_context_usage(t: &Scenario<'_>) {
    let context_conversation = t.conversation().await;
    seed_usage(t.pool, context_conversation.id, 40_000, 10_000).await;

    let context_page = t.tab(t.url(&format!("conversation/{}", context_conversation.id))).await;
    assert!(
        wait_for_text(&context_page, "25% of context", Duration::from_secs(10)).await,
        "the always-visible indicator should reflect the seeded usage \
         (40_000 + 10_000 of a 200_000 default window = 25%)"
    );

    click_when_present(&context_page, ".context-usage-bar", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&context_page, "Tools (", Duration::from_secs(10)).await,
        "clicking the indicator should open the detail view, listing every available tool"
    );
    assert!(
        wait_for_text(
            &context_page,
            "Tokens — input: 40000, output: 10000",
            Duration::from_secs(5)
        )
        .await,
        "the detail view should show the same real usage numbers the indicator did"
    );
    // The system prompt keeps its line breaks, and the dialog doesn't
    // scroll sideways: it was one run-together paragraph in a dialog
    // that scrolled both ways (SME-41 D7).
    let prompt_view: Vec<String> = context_page
        .evaluate(
            "(() => { const p = document.querySelector('.context-detail-prompt'); const panel = document.querySelector('.context-detail-panel'); \
             return [p ? getComputedStyle(p).whiteSpace : 'missing', String(panel.scrollWidth - panel.clientWidth)]; })()",
        )
        .await
        .expect("inspect the detail view")
        .into_value()
        .expect("strings");
    assert_eq!(prompt_view[0], "pre-wrap", "the system prompt should keep its line breaks: {prompt_view:?}");
    assert!(prompt_view[1].parse::<f64>().unwrap_or(1.0) <= 0.0, "the detail view scrolls sideways: {prompt_view:?}");
}

/// Scenario 6: a compaction event renders as a distinct, collapsed-by-default
/// divider — not an ordinary chat bubble — and expands to reveal the real
/// summary text on click. Seeded directly (a real trigger/summarization
/// round trip is already covered by turn's own mock-upstream
/// integration test; this tier's job is the DOM, not the backend logic).
pub(super) async fn scenario_compaction_divider(t: &Scenario<'_>) {
    let pool = t.pool;
    let conversation = t.conversation().await;
    seed_usage(pool, conversation.id, 40_000, 10_000).await;
    db::create_message(
        pool,
        conversation.id,
        "assistant",
        &[anthropic::ContentBlock::CompactionSummary {
            summary: "the user said hello; nothing else happened".to_string(),
            covers_through_message_id: 1,
        }],
    )
    .await
    .expect("seed a compaction summary message");

    let context_page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&context_page, conversation.id).await;
    assert!(
        wait_for_text(&context_page, "Conversation compacted", Duration::from_secs(10)).await,
        "a CompactionSummary block should render as its own distinct divider"
    );
    assert!(
        wait_for_text_gone(
            &context_page,
            "the user said hello; nothing else happened",
            Duration::from_secs(2)
        )
        .await,
        "collapsed by default — the summary text itself shouldn't be visible yet"
    );
    click_when_present(
        &context_page,
        ".compaction-summary-header",
        Duration::from_secs(5),
    )
    .await;
    // The open state first: if it's closed, the click missed or
    // something closed it (SME-68), which the text alone can't tell.
    assert!(
        wait_for_count(&context_page, ".compaction-summary-block[open]", 1, Duration::from_secs(5)).await,
        "the divider should be open after clicking its header"
    );
    assert!(
        wait_for_text(
            &context_page,
            "the user said hello; nothing else happened",
            Duration::from_secs(5)
        )
        .await,
        "expanding the divider should reveal the real summary text"
    );
}

/// Scenario 7: the todo panel — cold-load population from a seeded list,
/// then a live, full-replace update with no reload, via the real todowrite
/// tool (not a hand-built event) — see SME-20.
pub(super) async fn scenario_todo_panel(t: &Scenario<'_>) {
    let pool = t.pool;
    let todo_conversation = t.conversation().await;
    db::set_conversation_todos(
        pool,
        todo_conversation.id,
        &[
            anthropic::tools::TodoItem {
                content: "write the plan".to_string(),
                status: anthropic::tools::TodoStatus::Completed,
            },
            anthropic::tools::TodoItem {
                content: "implement".to_string(),
                status: anthropic::tools::TodoStatus::InProgress,
            },
        ],
    )
    .await
    .expect("seed initial todos");

    let todo_page = t.tab(t.url(&format!("conversation/{}", todo_conversation.id))).await;
    assert!(
        wait_for_text(&todo_page, "write the plan", Duration::from_secs(10)).await,
        "cold load should show the seeded todo list"
    );
    assert!(
        wait_for_text(&todo_page, "implement", Duration::from_secs(5)).await,
        "cold load should show every seeded item, not just the first"
    );

    anthropic::tools::execute(
        pool,
        todo_conversation.id,
        "toolu_todo_test",
        "todowrite",
        &serde_json::json!({"todos": [{"content": "ship it", "status": "pending"}]}),
    )
    .await
    .expect("todowrite should succeed");

    assert!(
        wait_for_text(&todo_page, "ship it", Duration::from_secs(10)).await,
        "a live todowrite call should update the panel with no reload"
    );
    assert!(
        wait_for_text_gone(&todo_page, "write the plan", Duration::from_secs(5)).await,
        "todowrite is a full replace — the old list shouldn't still be showing"
    );
}

/// Seeds `conversation` with one user message saying `text`, which also
/// titles it.
pub(super) async fn seed_user_message(pool: &sqlx::PgPool, conversation: i64, text: &str) {
    db::create_message(
        pool,
        conversation,
        "user",
        &[anthropic::ContentBlock::Text { text: text.to_string() }],
    )
    .await
    .expect("seed a user message");
}

/// Types `text` into the page's message box and sends it.
pub(super) async fn send_from(page: &chromiumoxide::Page, text: &str) {
    let input = wait_for_element(page, CHAT_INPUT, Duration::from_secs(10)).await;
    input.focus().await.expect("focus the message box");
    input.type_str(text).await.expect("type a message");
    input.press_key("Enter").await.expect("send it");
}

/// Scenario 8: a reply streaming into one conversation stays in that
/// conversation. Switching to another mid-stream must leave the other one
/// usable (not disabled while the first one's reply is in flight) and must
/// never show the first one's reply there; going back shows the finished
/// reply, once. The model is a slow mock upstream, so the switch lands
/// mid-stream.
pub(super) async fn scenario_reply_stays_in_its_conversation(t: &Scenario<'_>) {
    let pool = t.pool;
    let streaming = t.conversation().await;
    let other = t.conversation().await;
    seed_user_message(pool, other.id, "seeded message in B").await;

    let chat = t.tab(t.url(&format!("conversation/{}", streaming.id))).await;
    wait_for_live_client(&chat, streaming.id).await;
    send_from(&chat, "hello from A").await;
    assert!(
        wait_for_text(&chat, "zebra0", Duration::from_secs(10)).await,
        "A's reply should start streaming"
    );

    click_conversation(&chat, other.id).await;
    assert!(
        wait_for_text(&chat, "seeded message in B", Duration::from_secs(5)).await,
        "should now be showing B"
    );
    // A's sidebar row says it's working. Its going away below is how this
    // tab shows it has had the events after A's turn ended.
    let a_busy = format!(".conversation-item[data-conversation-id='{}'] .conversation-busy", streaming.id);
    assert!(
        wait_for_count(&chat, &a_busy, 1, Duration::from_secs(5)).await,
        "the sidebar should mark A as working while its reply streams"
    );
    for moment in ["right after switching", "a second later"] {
        let b = view_state(&chat).await;
        assert!(b.input_enabled, "B's message box is disabled {moment}: {b:?}");
        assert!(!b.streaming_bubble, "B shows a streaming bubble {moment}: {b:?}");
        assert!(!b.shows_reply, "B shows A's reply {moment}: {b:?}");
        // Five more of A's words (a second's worth) go out meanwhile.
        let sent = t.mock.chunks_sent();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while t.mock.chunks_sent() < sent + 5 && t.mock.open() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    // Let A's reply finish, and be saved, while B is still on screen.
    assert!(t.mock.wait_until_idle(Duration::from_secs(15)).await, "A's reply never finished upstream");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let saved = db::list_messages(pool, streaming.id).await.expect("read A's messages");
        if saved.iter().any(|m| m.role == "assistant" && m.content.contains("zebra24")) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "A's finished reply was never saved");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Saved isn't shown: the turn's events still have to reach this tab. A
    // turn ends after its reply is saved, and this tab hears of it on B's
    // own stream (the sidebar's busy mark), so once the mark is gone,
    // anything of A's that was going to land in B has had its chance.
    assert!(
        wait_for_count(&chat, &a_busy, 0, Duration::from_secs(10)).await,
        "A's busy mark never went away after its turn ended"
    );
    let b = view_state(&chat).await;
    assert!(!b.shows_reply, "A's finished reply landed in B: {b:?}");
    assert!(b.input_enabled, "B's message box is disabled after A finished: {b:?}");

    click_conversation(&chat, streaming.id).await;
    assert!(
        wait_for_text(&chat, "zebra24", Duration::from_secs(10)).await,
        "A should show its finished reply after switching back"
    );
    let copies: usize = chat
        .evaluate("document.querySelector('.messages').innerText.split('zebra24').length - 1")
        .await
        .expect("count the reply")
        .into_value()
        .expect("a number");
    assert_eq!(copies, 1, "A's reply should appear exactly once");
    // A's first message titled it; the sidebar should show that
    // without a reload.
    let title_selector = format!(
        ".conversation-item[data-conversation-id=\"{}\"] .conversation-title",
        streaming.id
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let title: String = chat
            .evaluate(format!("document.querySelector({title_selector:?})?.innerText ?? ''"))
            .await
            .expect("read A's sidebar title")
            .into_value()
            .expect("a string");
        if title.contains("hello from A") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "A's sidebar title is still {title:?} after its first message"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Scenario 9: a reply the watching tab didn't ask for — a finished
/// command's notice, another tab's send — still reaches it live, with no
/// reload.
pub(super) async fn scenario_unrequested_reply(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let watcher = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&watcher, conversation.id).await;
    crate::turn::run_turn(
        t.pool,
        conversation.id,
        anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "sent from somewhere else".to_string(),
            }],
        },
    )
    .await
    .expect("a turn run outside the watching tab should succeed");
    assert!(
        wait_for_text(&watcher, "zebra24", Duration::from_secs(10)).await,
        "the watching tab never showed a reply it didn't send itself"
    );
}
