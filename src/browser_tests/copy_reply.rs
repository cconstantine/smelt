use super::*;

/// SME-105: a finished reply copies as the markdown the model wrote, from
/// one button on its last bubble, which outlines every bubble it copies.
/// It copies over plain HTTP too, where `navigator.clipboard` doesn't
/// exist, and doesn't show on a reply still being written.
pub(super) async fn scenario_copy_reply(t: &Scenario<'_>) {
    let text = |text: &str| anthropic::ContentBlock::Text { text: text.to_string() };
    let part_one = "Let me check the todo list.";
    let part_two = "## Todos\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```text\nnothing yet\n```";
    let second_reply = "You're **welcome**.";
    let conversation = t.conversation().await;
    seed_user_message(t.pool, conversation.id, "Check the todos").await;
    db::create_message(t.pool, conversation.id, "assistant", &[
        text(&format!("\n{part_one}\n")),
        anthropic::ContentBlock::ToolUse { id: "toolu_copy".into(), name: "todoread".into(), input: serde_json::json!({}) },
    ])
    .await
    .expect("seed part one");
    db::create_message(t.pool, conversation.id, "user", &[anthropic::ContentBlock::ToolResult {
        tool_use_id: "toolu_copy".into(),
        content: "tool output that isn't copied".into(),
        is_error: None,
    }])
    .await
    .expect("seed the tool result");
    let reply = db::create_message(t.pool, conversation.id, "assistant", &[text(part_two)]).await.expect("seed part two").id;
    seed_user_message(t.pool, conversation.id, "Thanks").await;
    let later = db::create_message(t.pool, conversation.id, "assistant", &[text(second_reply)]).await.expect("seed a second reply").id;
    // A conversation to switch to and back (the outline mustn't outlast
    // its button, code review 1).
    let elsewhere = t.conversation().await;
    // Checks added in code review 1, collected so one run shows each.
    let mut problems: Vec<String> = Vec::new();
    let path = format!("conversation/{}", conversation.id);
    let page = t.tab(t.url(&path)).await;
    wait_for_live_client(&page, conversation.id).await;
    assert!(wait_for_count(&page, ".reply-copy", 2, Duration::from_secs(10)).await, "one button per finished reply");

    scenario_copy_reply_placement_and_indicator(later, &page, part_one, reply).await;
    scenario_copy_reply_copying(t, &conversation, &elsewhere, later, &page, part_one, part_two, &path, &mut problems, reply).await;
    scenario_copy_reply_phone_width(t, &page, part_one, &mut problems, reply).await;
    scenario_copy_reply_while_written(t, &mut problems, text).await;
}


/// Where each reply's Copy button sits, and the outline that shows what it copies.
async fn scenario_copy_reply_placement_and_indicator(later: i64, page: &chromiumoxide::Page, part_one: &str, reply: i64) {
    // Placement: on each reply's last bubble, in its footer, nowhere
    // interactive; part one is marked as the first reply's too.
    let facts: serde_json::Value = page.evaluate(REPLY_COPY_FACTS).await.expect("read the buttons").into_value().expect("facts");
    let (reply_id, later_id) = (reply.to_string(), later.to_string());
    assert_eq!(
        facts["buttons"],
        serde_json::json!([
            { "reply": reply_id, "label": "Copy reply (2 parts)", "title": "Copy this reply's 2 outlined parts as Markdown", "type": "button", "inFooter": true, "nested": false, "streaming": false },
            { "reply": later_id, "label": "Copy reply", "title": "Copy this reply as Markdown", "type": "button", "inFooter": true, "nested": false, "streaming": false },
        ]),
        "{facts}"
    );
    let marks: Vec<serde_json::Value> = facts["bubbles"].as_array().cloned().unwrap_or_default().iter().map(|b| b["reply"].clone()).collect();
    assert_eq!(marks, vec![serde_json::json!(reply_id), serde_json::json!(reply_id), serde_json::json!(later_id)], "{facts}");

    // The indicator: pointing at the button outlines exactly what it
    // copies, and moving away clears it.
    wait_for_outlined(&page, &[], Duration::from_secs(2)).await;
    let centre = |page: chromiumoxide::Page, selector: String| async move {
        let (x, y, w, h) = element_box(&page, &selector).await;
        chromiumoxide::layout::Point::new((x + w / 2) as f64, (y + h / 2) as f64)
    };
    page.move_mouse(centre(page.clone(), reply_copy_button(reply)).await).await.expect("point at the button");
    wait_for_outlined(&page, &[part_one, "Todos"], Duration::from_secs(3)).await;
    page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0)).await.expect("point away");
    wait_for_outlined(&page, &[], Duration::from_secs(3)).await;
}

/// Copying on the secure origin, from the keyboard, across a switch, refused, and over plain HTTP, and what reached the clipboard.
async fn scenario_copy_reply_copying(t: &Scenario<'_>, conversation: &crate::models::Conversation, elsewhere: &crate::models::Conversation, later: i64, page: &chromiumoxide::Page, part_one: &str, part_two: &str, path: &str, problems: &mut Vec<String>, reply: i64) {
    // The secure path (the tier's 127.0.0.1 origin): writeText gets the
    // reply's markdown, both parts, byte for byte.
    page.evaluate("navigator.clipboard.writeText = async text => { window.__smeltCopied = text; }")
        .await
        .expect("watch the clipboard");
    click_when_present(&page, &reply_copy_button(reply), Duration::from_secs(5)).await;
    assert_eq!(wait_for_copy_label(&page, reply, "Copied", Duration::from_secs(5)).await, "Copied");
    let copied: Option<String> = page.evaluate("window.__smeltCopied ?? null").await.expect("read the copy").into_value().expect("a string");
    let whole = format!("{part_one}\n\n{part_two}");
    assert_eq!(copied.as_deref(), Some(whole.as_str()));
    // While "Copied" shows, the outline stays even with the pointer away
    // (a tap on a phone has no hover).
    page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0)).await.expect("point away");
    wait_for_outlined(&page, &[part_one, "Todos"], Duration::from_secs(1)).await;
    assert_eq!(
        wait_for_copy_label(&page, reply, "Copy reply (2 parts)", Duration::from_secs(4)).await,
        "Copy reply (2 parts)",
        "the label goes back after a moment"
    );
    wait_for_outlined(&page, &[], Duration::from_secs(3)).await;

    // Focus from the keyboard (Tab from the code block's Copy, just before
    // it) outlines the reply, after a mouse click on the button (code
    // reviews 1 and 2).
    let code_copy = format!(".message[data-reply=\"{reply}\"] .md-code-copy");
    page.evaluate(format!("document.querySelector({code_copy:?}).focus()")).await.expect("focus the code block's Copy");
    page.find_element(code_copy.as_str()).await.expect("the code block's Copy").press_key("Tab").await.expect("press Tab");
    let on_button: bool = page
        .evaluate("document.activeElement?.classList.contains('reply-copy') ?? false")
        .await
        .expect("read the focus")
        .into_value()
        .expect("a bool");
    assert!(on_button, "Tab should reach the Copy reply button");
    if let Err(why) = outlined_within(&page, &[part_one, "Todos"], Duration::from_secs(2)).await {
        problems.push(format!("keyboard focus doesn't outline the reply: {why}"));
    }
    page.evaluate("document.activeElement?.blur()").await.expect("blur");
    wait_for_outlined(&page, &[], Duration::from_secs(3)).await;

    // Switching away while "Copied" shows, then back: the outline went
    // with its button (code review 1, L3).
    click_when_present(&page, &reply_copy_button(later), Duration::from_secs(5)).await;
    assert_eq!(wait_for_copy_label(&page, later, "Copied", Duration::from_secs(5)).await, "Copied");
    page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0)).await.expect("point away");
    click_conversation(&page, elsewhere.id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    click_conversation(&page, conversation.id).await;
    assert!(wait_for_count(&page, ".reply-copy", 2, Duration::from_secs(10)).await, "back on the conversation");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    if let Err(why) = outlined_within(&page, &[], Duration::from_secs(1)).await {
        problems.push(format!("an outline outlived its button: {why}"));
    }

    // A refused copy says so: writeText rejecting and execCommand failing.
    page.evaluate(
        "navigator.clipboard.writeText = async () => { throw new Error('denied'); }; \
         window.__smeltExec = document.execCommand; document.execCommand = () => false;",
    )
    .await
    .expect("refuse the clipboard");
    click_when_present(&page, &reply_copy_button(later), Duration::from_secs(5)).await;
    assert_eq!(wait_for_copy_label(&page, later, "Couldn't copy", Duration::from_secs(5)).await, "Couldn't copy");
    assert_eq!(
        page.evaluate("document.querySelectorAll('textarea').length").await.expect("count").into_value::<i64>().expect("a number"),
        0,
        "the fallback's textarea is removed even when the copy fails"
    );
    // The fallback ran after writeText was refused, outside the click:
    // its focus moves still aren't keyboard focus (code review 2, L2).
    page.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0)).await.expect("point away");
    assert_eq!(wait_for_copy_label(&page, later, "Copy reply", Duration::from_secs(4)).await, "Copy reply");
    if let Err(why) = outlined_within(&page, &[], Duration::from_secs(2)).await {
        problems.push(format!("the outline stayed after a refused writeText: {why}"));
    }

    // Over plain HTTP: no secure context and no Clipboard API, so the
    // fallback copies, without moving the transcript or the focus.
    let port = t.harness.base_url.trim_end_matches('/').rsplit(':').next().unwrap_or_default().to_string();
    let insecure = t.tab(format!("http://{INSECURE_HOST}:{port}/{path}")).await;
    wait_for_live_client(&insecure, conversation.id).await;
    let context: serde_json::Value = insecure
        .evaluate("({ secure: window.isSecureContext, clipboard: typeof navigator.clipboard })")
        .await
        .expect("read the context")
        .into_value()
        .expect("facts");
    assert_eq!(context, serde_json::json!({ "secure": false, "clipboard": "undefined" }), "not a secure context");
    assert!(wait_for_count(&insecure, ".reply-copy", 2, Duration::from_secs(10)).await);
    // Every scroll of the transcript or the page from the moment the click
    // lands (the click itself may scroll the button into view first). The
    // end positions alone can't show a jump: the transcript's
    // stick-to-bottom and pointer anchoring put it back afterwards.
    insecure
        .evaluate(
            "(() => { const m = document.querySelector('.messages'); const spacer = document.createElement('div'); \
             spacer.style.cssText = 'height:3000px;flex:none'; m.prepend(spacer); \
             m.scrollTop = m.scrollHeight; window.__smeltTranscriptTop = m.scrollTop; \
             const seen = what => () => { if (window.__smeltScrolls) window.__smeltScrolls.push([what, m.scrollTop, window.scrollY]); }; \
             m.addEventListener('scroll', seen('transcript')); window.addEventListener('scroll', seen('page')); \
             document.addEventListener('click', () => { window.__smeltScrolls = []; }, true); })()",
        )
        .await
        .expect("scroll the transcript");
    click_when_present(&insecure, &reply_copy_button(reply), Duration::from_secs(5)).await;
    assert_eq!(wait_for_copy_label(&insecure, reply, "Copied", Duration::from_secs(5)).await, "Copied");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let scrolls: serde_json::Value =
        insecure.evaluate("window.__smeltScrolls ?? null").await.expect("read the scrolls").into_value().expect("scrolls");
    let top: f64 = insecure.evaluate("window.__smeltTranscriptTop").await.expect("read").into_value().expect("a number");
    assert!(top > 2000.0, "the transcript should be scrolled well down before the copy: {top}");
    assert_eq!(scrolls, serde_json::json!([]), "copying scrolled the transcript or the page");
    let focus: serde_json::Value = insecure
        .evaluate("({ button: document.activeElement?.classList.contains('reply-copy') ?? false, areas: document.querySelectorAll('textarea').length })")
        .await
        .expect("read the focus")
        .into_value()
        .expect("facts");
    assert_eq!(focus, serde_json::json!({ "button": true, "areas": 0 }), "focus goes back to the button");
    // The fallback's focus moves aren't keyboard focus: once the pointer
    // leaves and "Copied" is over, the outline goes (code review 1, L2).
    insecure.move_mouse(chromiumoxide::layout::Point::new(5.0, 5.0)).await.expect("point away");
    assert_eq!(
        wait_for_copy_label(&insecure, reply, "Copy reply (2 parts)", Duration::from_secs(4)).await,
        "Copy reply (2 parts)"
    );
    if let Err(why) = outlined_within(&insecure, &[], Duration::from_secs(2)).await {
        problems.push(format!("the outline stayed after a plain-HTTP copy: {why}"));
    }
    // What reached the real clipboard, read from the secure origin.
    t.harness
        .browser
        .execute(
            chromiumoxide::cdp::browser_protocol::browser::GrantPermissionsParams::builder()
                .permission(chromiumoxide::cdp::browser_protocol::browser::PermissionType::ClipboardReadWrite)
                .permission(chromiumoxide::cdp::browser_protocol::browser::PermissionType::ClipboardSanitizedWrite)
                .origin(t.url("").trim_end_matches('/').to_string())
                .build()
                .expect("permission params"),
        )
        .await
        .expect("let the secure origin read the clipboard");
    let reader = t.tab(t.url("")).await;
    reader.bring_to_front().await.expect("focus the reader");
    let pasted: String = reader
        .evaluate("navigator.clipboard.readText()")
        .await
        .expect("read the clipboard")
        .into_value()
        .expect("a string");
    assert_eq!(pasted, whole, "the clipboard should hold the reply's markdown");
}

/// At phone width: the button stays in its bubble, and a tap or a touch copies.
async fn scenario_copy_reply_phone_width(t: &Scenario<'_>, page: &chromiumoxide::Page, part_one: &str, problems: &mut Vec<String>, reply: i64) {
    // The tier's browser is shared: later scenarios get no clipboard access.
    t.harness
        .browser
        .execute(chromiumoxide::cdp::browser_protocol::browser::ResetPermissionsParams::default())
        .await
        .expect("reset the permissions");

    // Phone width: the button stays inside its bubble and nothing scrolls
    // sideways.
    page.execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true))
        .await
        .expect("emulate a phone");
    let fits: serde_json::Value = page
        .evaluate(format!(
            "(() => {{ const b = document.querySelector({:?}).getBoundingClientRect(); \
             const m = document.querySelector({:?}).closest('.message').getBoundingClientRect(); \
             return {{ inside: b.left >= m.left && b.right <= m.right && b.top >= m.top && b.bottom <= m.bottom, \
             sideways: document.documentElement.scrollWidth > innerWidth }}; }})()",
            reply_copy_button(reply),
            reply_copy_button(reply)
        ))
        .await
        .expect("measure at phone width")
        .into_value()
        .expect("facts");
    assert_eq!(fits, serde_json::json!({ "inside": true, "sideways": false }), "{fits}");
    // A tap: "Copied" with the outline, and no outline once it's over
    // (a tap's focus isn't the keyboard's; code review 2, L1).
    page.evaluate(
        "navigator.clipboard.writeText = async text => { window.__smeltCopied = text; }; \
         if (window.__smeltExec) document.execCommand = window.__smeltExec; document.activeElement?.blur(); \
         document.querySelector('.reply-copy').closest('.message').scrollIntoView({ block: 'center' });",
    )
    .await
    .expect("let the clipboard work again");
    page.execute(chromiumoxide::cdp::browser_protocol::emulation::SetTouchEmulationEnabledParams::new(true))
        .await
        .expect("emulate touch");
    let (x, y, w, h) = element_box(&page, &reply_copy_button(reply)).await;
    // A touch start and end (headless Chrome turns these into a tap, with
    // its focus and click; `Input.synthesizeTapGesture` gives no click).
    {
        use chromiumoxide::cdp::browser_protocol::input::{DispatchTouchEventParams, DispatchTouchEventType, TouchPoint};
        let point = TouchPoint::new((x + w / 2) as f64, (y + h / 2) as f64);
        page.execute(DispatchTouchEventParams::new(DispatchTouchEventType::TouchStart, vec![point.clone()]))
            .await
            .expect("touch the button");
        // chromiumoxide leaves out an empty list, which Chrome refuses; the
        // point on the end still makes a tap.
        page.execute(DispatchTouchEventParams::new(DispatchTouchEventType::TouchEnd, vec![point]))
            .await
            .expect("lift the touch");
    }
    let tapped = wait_for_copy_label(&page, reply, "Copied", Duration::from_secs(5)).await;
    if tapped != "Copied" {
        problems.push(format!("the tap didn't copy (the button at {x},{y} {w}x{h} reads {tapped:?})"));
    } else {
        wait_for_outlined(&page, &[part_one, "Todos"], Duration::from_secs(1)).await;
        assert_eq!(
            wait_for_copy_label(&page, reply, "Copy reply (2 parts)", Duration::from_secs(4)).await,
            "Copy reply (2 parts)"
        );
        if let Err(why) = outlined_within(&page, &[], Duration::from_secs(2)).await {
            problems.push(format!("the outline stayed after a tap: {why}"));
        }
    }
}

/// While a reply is written, and after a reload mid-turn: no button until the reply is done.
async fn scenario_copy_reply_while_written(t: &Scenario<'_>, problems: &mut Vec<String>, text: impl Fn(&str) -> anthropic::ContentBlock) {
    use crate::events::{ConversationEvent, publish};
    // While a reply is written: no button on the streaming bubble, nor on
    // the newest reply's saved part; the earlier reply's still works. The
    // button shows on the reply's last part once the turn ends.
    let streaming = t.conversation().await;
    seed_user_message(t.pool, streaming.id, "first").await;
    let earlier = db::create_message(t.pool, streaming.id, "assistant", &[text("Earlier answer.")]).await.expect("seed").id;
    let live = t.tab(t.url(&format!("conversation/{}", streaming.id))).await;
    wait_for_live_client(&live, streaming.id).await;
    assert!(wait_for_count(&live, ".reply-copy", 1, Duration::from_secs(10)).await);
    publish(streaming.id, ConversationEvent::TurnState { running: true, elapsed: 0 });
    let asked = db::create_message(t.pool, streaming.id, "user", &[text("second")]).await.expect("save the ask");
    let partway = db::create_message(t.pool, streaming.id, "assistant", &[
        text("Saved before a call."),
        anthropic::ContentBlock::ToolUse { id: "toolu_wait".into(), name: "todoread".into(), input: serde_json::json!({}) },
    ])
    .await
    .expect("save part one");
    publish(streaming.id, ConversationEvent::MessagesAppended { messages: vec![asked, partway] });
    publish(streaming.id, ConversationEvent::ReplyReset {});
    publish(streaming.id, ConversationEvent::ReplyDelta { text: "Still streaming".into(), offset: 0 });
    assert!(wait_for_text(&live, "Still streaming", Duration::from_secs(10)).await);
    assert!(wait_for_text(&live, "Saved before a call.", Duration::from_secs(10)).await);
    let running: serde_json::Value = live.evaluate(REPLY_COPY_FACTS).await.expect("read the buttons").into_value().expect("facts");
    let buttons = running["buttons"].as_array().cloned().unwrap_or_default();
    assert_eq!(buttons.len(), 1, "only the finished reply has a button: {running}");
    assert_eq!(buttons[0]["reply"], serde_json::json!(earlier.to_string()), "{running}");
    live.evaluate("navigator.clipboard.writeText = async text => { window.__smeltCopied = text; }").await.expect("watch the clipboard");
    click_when_present(&live, &reply_copy_button(earlier), Duration::from_secs(5)).await;
    assert_eq!(wait_for_copy_label(&live, earlier, "Copied", Duration::from_secs(5)).await, "Copied");
    let copied: Option<String> = live.evaluate("window.__smeltCopied ?? null").await.expect("read").into_value().expect("a string");
    assert_eq!(copied.as_deref(), Some("Earlier answer."));
    let answered = db::create_message(t.pool, streaming.id, "user", &[anthropic::ContentBlock::ToolResult {
        tool_use_id: "toolu_wait".into(),
        content: "[]".into(),
        is_error: None,
    }])
    .await
    .expect("save the result");
    let finished = db::create_message(t.pool, streaming.id, "assistant", &[text("Finished now.")]).await.expect("save part two");
    let finished_id = finished.id;
    publish(streaming.id, ConversationEvent::MessagesAppended { messages: vec![answered, finished] });
    publish(streaming.id, ConversationEvent::TurnState { running: false, elapsed: 0 });
    assert!(wait_for_count(&live, ".reply-copy", 2, Duration::from_secs(10)).await, "the button shows once the turn ends");
    assert_eq!(
        wait_for_copy_label(&live, finished_id, "Copy reply (2 parts)", Duration::from_secs(5)).await,
        "Copy reply (2 parts)",
        "on the reply's last part, covering both"
    );

    // A reload mid-turn (a real turn, on the mock model) renders the same
    // on the server and the client: no hydration error, no button on the
    // running reply, and the button once it finishes.
    let reloading = t.conversation().await;
    seed_user_message(t.pool, reloading.id, "first").await;
    let before_turn = db::create_message(t.pool, reloading.id, "assistant", &[text("Before the turn.")]).await.expect("seed").id;
    let tab = t.tab("about:blank").await;
    let mut console = tab
        .event_listener::<chromiumoxide::cdp::js_protocol::runtime::EventConsoleApiCalled>()
        .await
        .expect("listen to the console");
    let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let heard = errors.clone();
    let listener = tokio::spawn(async move {
        while let Some(event) = futures_util::StreamExt::next(&mut console).await {
            if !matches!(event.r#type, chromiumoxide::cdp::js_protocol::runtime::ConsoleApiCalledType::Error) {
                continue;
            }
            let line = event
                .args
                .iter()
                .map(|arg| arg.value.as_ref().map(|v| v.to_string()).or_else(|| arg.description.clone()).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" ");
            heard.lock().expect("the console lock").push(line);
        }
    });
    tab.evaluate("console.error('smelt console probe')").await.expect("probe the console");
    tab.goto(t.url(&format!("conversation/{}", reloading.id))).await.expect("open the conversation");
    wait_for_live_client(&tab, reloading.id).await;
    send_from(&tab, "go").await;
    assert!(wait_for_text(&tab, "zebra1", Duration::from_secs(10)).await, "the reply should start streaming");
    // A part of the running reply already saved (as before a tool call):
    // the server renders the page without knowing the turn runs, so this
    // is where the server's and the client's renders could differ.
    let saved_part = db::create_message(t.pool, reloading.id, "assistant", &[text("Saved mid-turn.")]).await.expect("save a part");
    let saved_id = saved_part.id;
    publish(reloading.id, ConversationEvent::MessagesAppended { messages: vec![saved_part] });
    assert!(wait_for_text(&tab, "Saved mid-turn.", Duration::from_secs(5)).await);
    tab.reload().await.expect("reload mid-turn");
    wait_for_live_client(&tab, reloading.id).await;
    assert!(wait_for_count(&tab, ".stop-turn", 1, Duration::from_secs(5)).await, "still running after the reload");
    let mid: serde_json::Value = tab.evaluate(REPLY_COPY_FACTS).await.expect("read the buttons").into_value().expect("facts");
    assert_eq!(
        mid["buttons"].as_array().map(|b| b.iter().map(|b| b["reply"].clone()).collect::<Vec<_>>()),
        Some(vec![serde_json::json!(before_turn.to_string())]),
        "only the finished reply has a button mid-turn: {mid}"
    );
    let marks: Vec<serde_json::Value> = mid["bubbles"].as_array().cloned().unwrap_or_default().iter().map(|b| b["reply"].clone()).collect();
    assert!(
        !marks.contains(&serde_json::json!(saved_id.to_string())) && mid["bubbles"].to_string().contains("Saved mid-turn."),
        "the running reply's saved part has no button or mark after a reload: {mid}"
    );
    assert!(wait_for_count(&tab, ".stop-turn", 0, Duration::from_secs(20)).await, "the turn should finish");
    assert!(wait_for_count(&tab, ".reply-copy", 2, Duration::from_secs(10)).await, "the finished reply gets its button");
    listener.abort();
    let lines = errors.lock().expect("the console lock").clone();
    assert!(lines.iter().any(|l| l.contains("smelt console probe")), "the console listener heard nothing: {lines:?}");
    let console_errors: Vec<&String> = lines.iter().filter(|l| !l.contains("smelt console probe")).collect();
    assert!(console_errors.is_empty(), "console errors around a reload mid-turn: {console_errors:?}");
    assert!(problems.is_empty(), "{} problem(s): {problems:#?}", problems.len());
}
