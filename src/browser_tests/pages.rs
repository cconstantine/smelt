use super::*;

/// Scenario 18: a URL that isn't a page says so, with a way back. It used
/// to show the router's raw "Failed to parse route" dump (SME-40 F10).
pub(super) async fn scenario_not_found(t: &Scenario<'_>) {
    for path in ["nope", "conversation/abc"] {
        let page = t.tab(t.url(path)).await;
        assert!(
            wait_for_text(&page, "Page not found", Duration::from_secs(10)).await,
            "/{path} should say the page doesn't exist"
        );
        let body: String = page.evaluate("document.body.innerText").await.expect("read").into_value().expect("text");
        assert!(!body.contains("Failed to parse route"), "/{path} shows the router's debug output: {body}");
        click_when_present(&page, "a[href='/']", Duration::from_secs(5)).await;
        assert!(
            wait_for_count(&page, ".conversation-list", 1, Duration::from_secs(10)).await,
            "the link back should reach the conversations"
        );
        page.close().await.expect("close the tab");
    }
}

/// A conversation holding one successful and one failed tool call, for the
/// tool rows.
pub(super) async fn conversation_with_tool_calls(t: &Scenario<'_>) -> crate::models::Conversation {
    let tools = t.conversation().await;
    let call = |id: &str, name: &str, input: serde_json::Value| anthropic::ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
    };
    let result = |id: &str, content: &str, is_error: bool| anthropic::ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: content.to_string(),
        is_error: is_error.then_some(true),
    };
    db::create_message(t.pool, tools.id, "assistant", &[
        call("toolu_ok", "run_terminal_command", serde_json::json!({"command": "ls /tmp", "terminal_id": 1})),
        call("toolu_bad", "read_file", serde_json::json!({"path": "/nope.txt"})),
    ]).await.expect("seed the calls");
    db::create_message(t.pool, tools.id, "user", &[
        result("toolu_ok", "command sent (id: abc)", false),
        result("toolu_bad", "No such file or directory", true),
    ]).await.expect("seed the results");
    tools
}

/// Scenario 19: a tool call reads as one compact line that says what it
/// did, with its result folded in; a failed one is open. Each call and each
/// result used to be its own card of raw JSON, 24 of them for a three-line
/// answer (SME-41 D2).
pub(super) async fn scenario_tool_rows(t: &Scenario<'_>) {
    let tools = conversation_with_tool_calls(t).await;
    let page = t.tab(t.url(&format!("conversation/{}", tools.id))).await;
    wait_for_live_client(&page, tools.id).await;
    assert!(wait_for_count(&page, ".tool-row", 2, Duration::from_secs(10)).await, "one row per call");
    // With no usage yet, the meter says so in words (SME-41 D11).
    assert!(
        wait_for_text(&page, "No usage yet", Duration::from_secs(5)).await,
        "the context meter should say there's no usage yet"
    );
    let rows: Vec<(String, bool)> = page
        .evaluate("Array.from(document.querySelectorAll('.tool-row')).map(r => [r.querySelector('summary').innerText, r.open])")
        .await
        .expect("read the rows")
        .into_value()
        .expect("rows");
    assert!(rows[0].0.contains("Ran `ls /tmp`") && !rows[0].1, "a successful call is one closed line: {rows:?}");
    assert!(rows[1].0.contains("Read /nope.txt") && rows[1].1, "a failed call is open: {rows:?}");
    assert_eq!(
        page.evaluate("document.querySelectorAll('.tool-result, .tool-call').length").await.expect("count").into_value::<i64>().expect("n"),
        0,
        "results shouldn't be separate cards any more"
    );
    let body: String = page.evaluate("document.querySelector('.tool-row').textContent").await.expect("read").into_value().expect("text");
    assert!(body.contains("command sent"), "the raw result is still there on expand: {body}");
}

/// Scenario 20: dark mode follows the system setting. There was none: a
/// dark-mode system got a bright white page (SME-41 D5).
pub(super) async fn scenario_dark_mode(t: &Scenario<'_>) {
    let tools = conversation_with_tool_calls(t).await;
    let page = t.tab("about:blank").await;
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetEmulatedMediaParams::builder()
            .feature(chromiumoxide::cdp::browser_protocol::emulation::MediaFeature::new(
                "prefers-color-scheme",
                "dark",
            ))
            .build(),
    )
    .await
    .expect("ask for dark mode");
    page.goto(t.url(&format!("conversation/{}", tools.id))).await.expect("open the conversation");
    wait_for_live_client(&page, tools.id).await;
    wait_for_element(&page, ".tool-row-text", Duration::from_secs(10)).await;
    let luminance: Vec<f64> = page
        .evaluate(
            "(() => { const lum = c => { const [r, g, b] = c.match(/\\d+/g).map(Number); return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255; }; \
             const bg = el => getComputedStyle(el).backgroundColor; \
             return [lum(bg(document.body)), lum(bg(document.querySelector('.sidebar'))), lum(getComputedStyle(document.querySelector('.tool-row-text')).color)]; })()",
        )
        .await
        .expect("measure the colours")
        .into_value()
        .expect("numbers");
    assert!(luminance[0] < 0.2 && luminance[1] < 0.25, "the page should be dark in dark mode: {luminance:?}");
    assert!(luminance[2] > 0.55, "text should be light in dark mode: {luminance:?}");
}

/// Scenario 21: one primary action per form, and intro text lines up with
/// its heading. Every button in the MCP form was solid black, "Remove"
/// included, and intro text sat 24px in from the heading (SME-41 D6).
pub(super) async fn scenario_forms(t: &Scenario<'_>) {
    let page = t.tab(t.url("mcp-servers")).await;
    wait_for_element(&page, "h1", Duration::from_secs(10)).await;
    let edges: Vec<f64> = page
        .evaluate(
            "(() => { const p = document.querySelector('.mcp-servers-page p.muted'); \
             return [document.querySelector('h1').getBoundingClientRect().left, \
             p.getBoundingClientRect().left + parseFloat(getComputedStyle(p).paddingLeft)]; })()",
        )
        .await
        .expect("measure")
        .into_value()
        .expect("numbers");
    assert!((edges[0] - edges[1]).abs() < 1.0, "intro text should line up with the heading: {edges:?}");
    page.goto(t.url("mcp-servers/new")).await.expect("open the new-server form");
    wait_for_element(&page, ".mcp-remove-header", Duration::from_secs(10)).await;
    let fills: Vec<String> = page
        .evaluate(
            "['button[type=submit]', '.mcp-add-header', '.mcp-remove-header'].map(s => getComputedStyle(document.querySelector(s)).backgroundColor)",
        )
        .await
        .expect("read the buttons")
        .into_value()
        .expect("colours");
    assert_ne!(fills[0], fills[1], "the main action should stand out from the secondary ones: {fills:?}");
    assert_eq!(fills[1], fills[2], "secondary actions should share one style: {fills:?}");
    // Each auth choice's radio button sits beside its label, not above
    // it (SME-41 D13).
    let radios: Vec<f64> = page
        .evaluate(
            "(() => { const r = document.querySelector('input[name=mcp-new-auth-mode]').getBoundingClientRect(); \
             const l = document.querySelector('input[name=mcp-new-auth-mode]').parentElement.getBoundingClientRect(); \
             return [r.top + r.height / 2, l.top + l.height / 2, l.height]; })()",
        )
        .await
        .expect("measure the radio")
        .into_value()
        .expect("numbers");
    assert!((radios[0] - radios[1]).abs() < 4.0 && radios[2] < 30.0, "a radio should sit beside its label: {radios:?}");
}

/// Scenario 22: a new conversation says what smelt can do and offers
/// example asks; one fills the message box without sending. It was a blank
/// screen (SME-41 D12).
pub(super) async fn scenario_empty_conversation(t: &Scenario<'_>) {
    let empty = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", empty.id))).await;
    wait_for_live_client(&page, empty.id).await;
    assert!(
        wait_for_count(&page, ".conversation-empty .example-ask", 3, Duration::from_secs(10)).await,
        "an empty conversation should offer example asks"
    );
    let example: String = page.evaluate("document.querySelector('.example-ask').innerText").await.expect("read").into_value().expect("text");
    click_when_present(&page, ".example-ask", Duration::from_secs(5)).await;
    let mut filled = String::new();
    for _ in 0..25 {
        filled = page.evaluate(format!("document.querySelector({CHAT_INPUT:?}).value")).await.expect("read").into_value().expect("text");
        if !filled.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(filled, example, "picking an example should fill the message box");
    assert_eq!(
        page.evaluate("document.querySelectorAll('.message-user').length").await.expect("count").into_value::<i64>().expect("n"),
        0,
        "picking an example shouldn't send it"
    );
}
