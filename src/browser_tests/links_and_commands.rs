use super::*;

/// Every link in the first element matching `selector`: its `href`, words,
/// `target` and `rel`.
pub(super) fn link_facts_script(selector: &str) -> String {
    format!(
        "Array.from(document.querySelector('{selector}')?.querySelectorAll('a') ?? []).map(a => \
         [a.getAttribute('href'), a.innerText, a.getAttribute('target'), a.getAttribute('rel')])"
    )
}

/// SME-104: a bare URL, a `www.` address and an email address in a reply
/// become links that open in a new tab, with `noopener noreferrer`, their
/// words unchanged; a URL in inline code, in a code block or with another
/// scheme stays text, and a markdown link stays one link.
pub(super) async fn scenario_markdown_bare_urls(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let reply = "See https://example.com/bare, www.example.org or mail me@example.com.\n\n\
        Not `https://example.com/code` nor ftp://example.com/x, but [the docs](https://example.com/docs).\n\n\
        ```\nhttps://example.com/block\n```\n";
    db::create_message(t.pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: reply.into() }])
        .await
        .expect("seed the reply");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    wait_for_element(&page, ".message-assistant .markdown a", Duration::from_secs(10)).await;
    let links: Vec<(String, String, String, String)> = page
        .evaluate(link_facts_script(".message-assistant .markdown"))
        .await
        .expect("read the reply's links")
        .into_value()
        .expect("links");
    let tab = |href: &str, words: &str| {
        (href.to_string(), words.to_string(), "_blank".to_string(), "noopener noreferrer".to_string())
    };
    assert_eq!(
        links,
        vec![
            tab("https://example.com/bare", "https://example.com/bare"),
            tab("https://www.example.org", "www.example.org"),
            tab("mailto:me@example.com", "me@example.com"),
            tab("https://example.com/docs", "the docs"),
        ]
    );
    let text: String = page
        .evaluate("document.querySelector('.message-assistant .markdown').innerText")
        .await
        .expect("read the reply")
        .into_value()
        .expect("text");
    for kept in ["https://example.com/code", "ftp://example.com/x", "https://example.com/block"] {
        assert!(text.contains(kept), "{kept} should still show as text: {text}");
    }
}

/// SME-104: a bare URL that arrives over two deltas is one link once it's
/// whole, and the reply looks the same once it's saved.
pub(super) async fn scenario_markdown_bare_urls_streaming(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    publish(conversation.id, ConversationEvent::ReplyReset {});
    let first = "The guide is at https://exa";
    publish(conversation.id, ConversationEvent::ReplyDelta { text: first.into(), offset: 0 });
    wait_for_element(&page, ".message-streaming .markdown a", Duration::from_secs(10)).await;
    let rest = "mple.com/guide. Read it.";
    publish(conversation.id, ConversationEvent::ReplyDelta { text: rest.into(), offset: first.len() });
    wait_for_element(&page, ".message-streaming .markdown a[href='https://example.com/guide']", Duration::from_secs(10))
        .await;
    let links: Vec<(String, String, String, String)> = page
        .evaluate(link_facts_script(".message-streaming .markdown"))
        .await
        .expect("read the streamed links")
        .into_value()
        .expect("links");
    let guide = "https://example.com/guide".to_string();
    assert_eq!(
        links,
        vec![(guide.clone(), guide, "_blank".to_string(), "noopener noreferrer".to_string())],
        "one link for the whole URL"
    );
    let streamed: String = page
        .evaluate("document.querySelector('.message-streaming .markdown').innerHTML.replace(/ data-dioxus-id=\"\\d+\"/g, '')")
        .await
        .expect("read the streamed reply")
        .into_value()
        .expect("html");
    let saved = db::create_message(
        t.pool,
        conversation.id,
        "assistant",
        &[anthropic::ContentBlock::Text { text: format!("{first}{rest}") }],
    )
    .await
    .expect("save the reply");
    publish(conversation.id, ConversationEvent::MessagesAppended { messages: vec![saved] });
    assert!(
        wait_for_count(&page, ".message-streaming", 0, Duration::from_secs(10)).await,
        "the saved reply should replace its streaming copy"
    );
    let saved_html: String = page
        .evaluate("document.querySelector('.message-assistant .markdown').innerHTML.replace(/ data-dioxus-id=\"\\d+\"/g, '')")
        .await
        .expect("read the saved reply")
        .into_value()
        .expect("html");
    assert_eq!(saved_html, streamed, "the reply changed when it was saved");
}

/// SME-104 review 2: a bidi control in the text before a link doesn't
/// reverse how the link shows. An RLO (U+202E) left open in a reply
/// reordered a following bare link `https://evil.com/moc.knab//:sptth`
/// to read `https://bank.com/moc.live//:sptth`. The link's first character
/// must show to the left of its last.
pub(super) async fn scenario_markdown_link_bidi_isolated(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    let reply = "Go to \u{202E}https://evil.com/moc.knab//:sptth now, or [this](https://evil.com/x) one.\n";
    db::create_message(t.pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: reply.into() }])
        .await
        .expect("seed the reply");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    wait_for_element(&page, ".message-assistant .markdown a", Duration::from_secs(10)).await;
    // For each link: its words, and the on-screen x of its first and last
    // characters.
    let order: Vec<(String, f64, f64)> = page
        .evaluate(
            "Array.from(document.querySelectorAll('.message-assistant .markdown a')).map(a => { \
               const node = a.firstChild.nodeType === 3 ? a.firstChild : a.firstChild.firstChild; \
               const at = i => { const r = document.createRange(); r.setStart(node, i); r.setEnd(node, i + 1); \
                 return r.getBoundingClientRect().left; }; \
               return [a.innerText, at(0), at(node.length - 1)]; })",
        )
        .await
        .expect("measure the links")
        .into_value()
        .expect("links");
    assert_eq!(order.len(), 2, "two links: {order:?}");
    for (words, first, last) in &order {
        assert!(first < last, "{words:?} shows reversed: its first character is at x={first}, its last at x={last}");
    }
}

/// The terminal titlebar's pill, as JSON (null when there's none, or it
/// can't be read): its text, classes, and whether
/// it lies inside the viewport, its terminal card and (when there is one)
/// `.side-panels-row`'s visible box, and whether the connection pill beside
/// it is either hidden or readable (at least 30px) inside the card.
pub(super) async fn command_pill(page: &chromiumoxide::Page) -> serde_json::Value {
    page.evaluate(
        "JSON.stringify((() => { const p = document.querySelector('.task-terminal-titlebar .task-terminal-command'); \
         if (!p) return null; \
         const r = p.getBoundingClientRect(); \
         const inside = (o) => r.top >= o.top - 0.5 && r.bottom <= o.bottom + 0.5 && r.left >= o.left - 0.5 && r.right <= o.right + 0.5; \
         const card = p.closest('.task-terminal').getBoundingClientRect(); \
         const row = document.querySelector('.side-panels-row'); \
         const status = p.closest('.task-terminal-titlebar').querySelector('.task-terminal-status').getBoundingClientRect(); \
         return { text: p.textContent, cls: p.className, \
           viewport: inside({ top: 0, left: 0, bottom: innerHeight, right: innerWidth }), \
           card: inside(card), row: row ? inside(row.getBoundingClientRect()) : null, \
           title: p.getAttribute('title'), \
           card_width: card.width, \
           status_shown: status.width > 0, \
           status_in_card: status.width === 0 || (status.left >= card.left - 0.5 && status.right <= card.right + 0.5 && status.width >= 30), \
           layout: [card.left, card.right].concat(Array.from(p.closest('.task-terminal-titlebar').querySelectorAll('*')) \
             .map(e => e.className + ':' + Math.round(e.getBoundingClientRect().left) + '-' + Math.round(e.getBoundingClientRect().right))) }; })())",
    )
    .await
    .expect("read the command pill")
    .into_value::<String>()
    .ok()
    .and_then(|json| serde_json::from_str(&json).ok())
    .unwrap_or(serde_json::Value::Null)
}

/// SME-144: the terminal titlebar's height, in CSS pixels to a hundredth.
/// The body below it scrolls, and a titlebar that changes height with the
/// pill resizes the body, which the terminal's follow takes for the window
/// resizing (it broke `terminal_follow_after_switch`'s return to the bottom).
pub(super) async fn titlebar_height(page: &chromiumoxide::Page) -> f64 {
    let height: f64 = page
        .evaluate("document.querySelector('.task-terminal-titlebar').getBoundingClientRect().height")
        .await
        .expect("measure the titlebar")
        .into_value()
        .expect("a number");
    (height * 100.0).round() / 100.0
}

/// Waits for the pill to contain `text` and carry `class`; returns it.
pub(super) async fn wait_for_pill(page: &chromiumoxide::Page, text: &str, class: &str, timeout: Duration) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let pill = command_pill(page).await;
        if pill["text"].as_str().is_some_and(|t| t.contains(text)) && pill["cls"].as_str().is_some_and(|c| c.contains(class)) {
            return pill;
        }
        assert!(tokio::time::Instant::now() < deadline, "the pill never read {text:?} ({class}); last: {pill}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// SME-144: each terminal's titlebar shows its last command's status in a
/// pill that stays in view while the output scrolls, live and after a
/// reload, at laptop and phone widths.
pub(super) async fn scenario_terminal_command_indicator(t: &Scenario<'_>) {
    let pool = t.pool;
    let conversation = t.conversation().await;
    sandbox::create_pod(pool, conversation.id, Default::default()).await.expect("create_pod");
    let terminal = sandbox::create_terminal(pool, conversation.id).await.expect("create_terminal");
    let run = |command: &'static str, label: &'static str| {
        let command_id = unique_id(label);
        async move {
            anthropic::tools::execute(
                pool,
                conversation.id,
                &command_id,
                "run_terminal_command",
                &serde_json::json!({"terminal_id": terminal, "command": command}),
            )
            .await
            .expect("run_terminal_command");
            command_id
        }
    };

    let page = t.tab("about:blank").await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(1400, 900, 1.0, false),
    )
    .await
    .expect("set the window size");
    let open = || async {
        page.goto(t.url(&format!("conversation/{}", conversation.id))).await.expect("open the conversation");
        wait_for_live_client(&page, conversation.id).await;
        // By its titlebar, not its name: a narrow card hides the name.
        assert!(
            wait_for_count(&page, ".task-terminal-titlebar", 1, Duration::from_secs(15)).await,
            "the terminal card should render"
        );
    };
    open().await;
    assert!(command_pill(&page).await.is_null(), "a terminal with no command shows no pill");
    let mut heights = vec![("no pill", titlebar_height(&page).await)];

    // Long output, then a pause, then a failure. A subshell: a bare `exit`
    // would end the terminal's own shell.
    run("for i in $(seq -w 1 200); do echo indicator-$i; done; sleep 6; (exit 3)", "cmd-long").await;
    assert!(wait_for_text(&page, "indicator-200", Duration::from_secs(30)).await, "the output should stream in");
    let pill = wait_for_pill(&page, "running", "task-terminal-command-running", Duration::from_secs(5)).await;
    let overflows: bool = page
        .evaluate("(() => { const b = document.querySelector('.task-terminal-body'); return b.scrollHeight > b.clientHeight + 40; })()")
        .await
        .expect("measure the body")
        .into_value()
        .expect("a bool");
    assert!(overflows, "the output doesn't overflow the terminal body, so the in-view checks measure nothing");
    assert_eq!((pill["viewport"].as_bool(), pill["card"].as_bool()), (Some(true), Some(true)), "{pill}");
    heights.push(("running", titlebar_height(&page).await));
    page.evaluate("document.querySelector('.task-terminal-body').scrollTop = 0").await.expect("scroll to the top");
    let pill = command_pill(&page).await;
    assert_eq!(pill["viewport"].as_bool(), Some(true), "scrolled to the top, the pill left the viewport: {pill}");
    let pill = wait_for_pill(&page, "exit 3", "task-terminal-command-failed", Duration::from_secs(20)).await;
    assert!(pill["title"].as_str().is_some_and(|t| t.contains("exited with status 3")), "{pill}");
    heights.push(("exit 3", titlebar_height(&page).await));
    wait_command_finished(pool, terminal).await;

    // A command that finishes at once: its start reaches the tab before
    // its finish, so the pill doesn't stick at running.
    run("true", "cmd-true").await;
    wait_for_pill(&page, "exit 0", "task-terminal-command-ok", Duration::from_secs(15)).await;
    heights.push(("exit 0", titlebar_height(&page).await));
    wait_command_finished(pool, terminal).await;

    // Running across a reload, then interrupted.
    let sleeping = run("sleep 30", "cmd-sleep").await;
    wait_for_pill(&page, "running", "task-terminal-command-running", Duration::from_secs(15)).await;
    open().await;
    wait_for_pill(&page, "running", "task-terminal-command-running", Duration::from_secs(15)).await;
    anthropic::tools::execute(
        pool,
        conversation.id,
        &unique_id("signal"),
        "send_signal",
        &serde_json::json!({"command_id": sleeping, "signal": "INT"}),
    )
    .await
    .expect("send_signal");
    let pill = wait_for_pill(&page, "exit", "task-terminal-command-failed", Duration::from_secs(15)).await;
    assert!(!pill["text"].as_str().unwrap_or_default().contains("exit 0"), "an interrupted command exited 0: {pill}");
    wait_command_finished(pool, terminal).await;

    // A lost command, seen after a reload.
    let lost = unique_id("cmd-lost");
    db::create_terminal_command(pool, conversation.id, terminal, &lost, "make", 300).await.expect("a command row");
    db::mark_terminal_command_lost(pool, &lost).await.expect("mark it lost");
    open().await;
    let pill = wait_for_pill(&page, "lost", "task-terminal-command-lost", Duration::from_secs(15)).await;
    assert!(pill["title"].as_str().is_some_and(|t| t.contains("no exit status")), "{pill}");
    heights.push(("lost", titlebar_height(&page).await));
    assert!(
        heights.iter().all(|(_, h)| *h == heights[0].1),
        "the titlebar changes height with the pill, which resizes the terminal body: {heights:?}"
    );

    // Between 720 and 1024px the side panels sit in a row, and a todo list
    // takes a fixed width beside the sandbox: a narrow titlebar keeps both
    // pills inside the card (SME-144 review 1).
    anthropic::tools::execute(
        pool,
        conversation.id,
        &unique_id("todos"),
        "todowrite",
        &serde_json::json!({"todos": [{"content": "narrow the window", "status": "pending"}]}),
    )
    .await
    .expect("todowrite");
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(740, 900, 1.0, false),
    )
    .await
    .expect("set a middle width");
    open().await;
    let narrow = |pill: serde_json::Value, width: u32| {
        assert!(
            pill["card_width"].as_f64().is_some_and(|w| w < 200.0),
            "at {width}px the card isn't narrow, so this checks nothing: {pill}"
        );
        assert_eq!(
            (pill["card"].as_bool(), pill["status_in_card"].as_bool()),
            (Some(true), Some(true)),
            "at {width}px beside a todo list, a pill falls outside the terminal's card or is a blank stub: {pill}"
        );
    };
    assert!(wait_for_count(&page, ".todo-panel", 1, Duration::from_secs(15)).await, "the todo panel should show");
    let pill = wait_for_pill(&page, "lost", "task-terminal-command-lost", Duration::from_secs(15)).await;
    narrow(pill, 740);
    assert_eq!(titlebar_height(&page).await, heights[0].1, "the titlebar's height changes at 740px");

    // At a phone's width the pill is on screen, inside the side panels'
    // strip.
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true),
    )
    .await
    .expect("emulate a phone");
    open().await;
    let pill = wait_for_pill(&page, "lost", "task-terminal-command-lost", Duration::from_secs(15)).await;
    assert_eq!(
        (pill["viewport"].as_bool(), pill["card"].as_bool(), pill["row"].as_bool()),
        (Some(true), Some(true), Some(true)),
        "at a phone's width: {pill}"
    );

    // The narrowest middle width, with the widest common pill (SME-144
    // review 2): running, at 721px beside the todo list.
    run("sleep 30", "cmd-narrow").await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(721, 900, 1.0, false),
    )
    .await
    .expect("set the narrowest middle width");
    open().await;
    assert!(wait_for_count(&page, ".todo-panel", 1, Duration::from_secs(15)).await, "the todo panel should show");
    let pill = wait_for_pill(&page, "running", "task-terminal-command-running", Duration::from_secs(15)).await;
    narrow(pill, 721);
    assert_eq!(titlebar_height(&page).await, heights[0].1, "the titlebar's height changes at 721px");

    let _ = sandbox::terminate_terminal(pool, terminal).await;
    let _ = sandbox::terminate_pod(pool, conversation.id).await;
}

/// B19 (SME-122): a turn's error goes once a later turn starts, in every
/// tab, not only the tab that sent it. Before, a success sent from another
/// tab left the first tab showing the error.
pub(super) async fn scenario_turn_error_clears_on_a_later_turn(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    publish(conversation.id, ConversationEvent::TurnError { message: "B19-TURN-ERROR".into() });
    assert!(wait_for_text(&page, "B19-TURN-ERROR", Duration::from_secs(10)).await, "the error never showed");

    // Another tab sends: its turn starts, and finishes without an error.
    publish(conversation.id, ConversationEvent::TurnState { running: true, elapsed: 0 });
    publish(conversation.id, ConversationEvent::TurnState { running: false, elapsed: 0 });

    assert!(
        wait_for_text_gone(&page, "B19-TURN-ERROR", Duration::from_secs(10)).await,
        "the earlier turn's error should go once another turn starts"
    );
}

/// B44 (SME-122): a conversation deleted in another tab leaves the open
/// tab saying so, without the transcript under the message, and with the
/// conversation gone from its sidebar. Before, the transcript stayed, and
/// the sidebar listed it (highlighted) until a reload.
pub(super) async fn scenario_deleted_in_another_tab(t: &Scenario<'_>) {
    let deleted = t.conversation().await;
    db::create_message(
        t.pool,
        deleted.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "B44-TRANSCRIPT-MARKER".to_string() }],
    )
    .await
    .expect("save a message");
    let other = t.conversation().await;
    let row = format!(".conversation-item[data-conversation-id='{}']", deleted.id);

    let open = t.tab(t.url(&format!("conversation/{}", deleted.id))).await;
    wait_for_live_client(&open, deleted.id).await;
    assert!(wait_for_text(&open, "B44-TRANSCRIPT-MARKER", Duration::from_secs(10)).await, "the transcript never showed");
    assert!(wait_for_count(&open, &row, 1, Duration::from_secs(10)).await, "the sidebar should list it");

    let deleter = t.tab(t.url(&format!("conversation/{}", other.id))).await;
    wait_for_live_client(&deleter, other.id).await;
    let delete_button = format!("{row} .delete-conversation");
    wait_for_stable(&deleter, &delete_button, tokio::time::Instant::now() + Duration::from_secs(10)).await;
    click_when_present(&deleter, &delete_button, Duration::from_secs(5)).await;
    click_when_present(&deleter, &format!("{delete_button}.confirm"), Duration::from_secs(5)).await;
    assert!(wait_for_count(&deleter, &row, 0, Duration::from_secs(10)).await, "the deleting tab's sidebar drops it");

    assert!(
        wait_for_text(&open, "doesn't exist", Duration::from_secs(15)).await,
        "the open tab should say the conversation doesn't exist"
    );
    assert!(
        wait_for_text_gone(&open, "B44-TRANSCRIPT-MARKER", Duration::from_secs(5)).await,
        "the deleted conversation's transcript shouldn't stay under the message"
    );
    assert!(
        wait_for_count(&open, &row, 0, Duration::from_secs(10)).await,
        "the open tab's sidebar shouldn't list the deleted conversation"
    );
}
