use super::*;

/// SME-82: the context bar and its detail view work from the
/// keyboard. The bar was a clickable div (no focus), the view had no dialog
/// role and ignored Escape, and its "×" had no name.
pub(super) async fn scenario_context_from_the_keyboard(t: &Scenario<'_>) {
    let keyboard_conversation = t.conversation().await;
    db::create_message(
        t.pool,
        keyboard_conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "hello".to_string() }],
    )
    .await
    .expect("seed a message");
    db::upsert_conversation_usage(
        t.pool,
        keyboard_conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 40_000,
            output_tokens: 10_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let keyboard_page = t.tab(t.url(&format!("conversation/{}", keyboard_conversation.id))).await;
    assert!(
        wait_for_text(&keyboard_page, "25% of context", Duration::from_secs(10)).await,
        "the context bar should show"
    );
    wait_for_live_client(&keyboard_page, keyboard_conversation.id).await;
    let focused_is = |selector: &str| format!("document.activeElement === document.querySelector({selector:?})");
    let bar = wait_for_element(&keyboard_page, ".context-usage-bar", Duration::from_secs(5)).await;
    bar.focus().await.expect("focus the context bar");
    let bar_focused: bool = keyboard_page
        .evaluate(focused_is(".context-usage-bar"))
        .await
        .expect("read the focus")
        .into_value()
        .expect("a bool");
    assert!(bar_focused, "the context bar should take keyboard focus");
    bar.press_key("Enter").await.expect("press Enter on the bar");
    assert!(
        wait_for_count(&keyboard_page, ".context-detail-overlay [role=dialog]", 1, Duration::from_secs(5)).await,
        "Enter on the bar should open the detail view as a dialog"
    );
    // Focus moves in a task the page spawns, so wait for it.
    assert!(
        wait_for_focus(&keyboard_page, ".context-detail-close", Duration::from_secs(5)).await,
        "opening the detail view should focus its close button"
    );
    let close_name: String = keyboard_page
        .evaluate("document.querySelector('.context-detail-close').getAttribute('aria-label') || ''")
        .await
        .expect("read the close button")
        .into_value()
        .expect("a string");
    assert_eq!(close_name, "Close", "the close button should be named Close");
    let close = wait_for_element(&keyboard_page, ".context-detail-close", Duration::from_secs(5)).await;
    close.press_key("Escape").await.expect("press Escape");
    assert!(
        wait_for_count(&keyboard_page, ".context-detail-overlay", 0, Duration::from_secs(5)).await,
        "Escape should close the detail view"
    );
    assert!(
        wait_for_focus(&keyboard_page, ".context-usage-bar", Duration::from_secs(5)).await,
        "closing the detail view should put focus back on the bar"
    );

    // Escape still closes the view after a click on its own text, which
    // moved focus to the page body, outside the view's key handler
    // (SME-82 code review).
    bar.press_key("Enter").await.expect("press Enter on the bar");
    assert!(
        wait_for_text(&keyboard_page, "Tools (", Duration::from_secs(10)).await,
        "the detail view should load"
    );
    click_when_present(&keyboard_page, ".context-detail-panel h3", Duration::from_secs(5)).await;
    bar.press_key("Escape").await.expect("press Escape");
    assert!(
        wait_for_count(&keyboard_page, ".context-detail-overlay", 0, Duration::from_secs(3)).await,
        "Escape should close the detail view after a click on its text"
    );
    // Enter goes to whatever has focus, and focus returns to the bar in
    // a task the page spawns.
    assert!(
        wait_for_focus(&keyboard_page, ".context-usage-bar", Duration::from_secs(5)).await,
        "closing the detail view should put focus back on the bar"
    );

    // Tab doesn't walk out of a view that says it's modal (SME-82 code
    // review).
    bar.press_key("Enter").await.expect("press Enter on the bar");
    assert!(
        wait_for_focus(&keyboard_page, ".context-detail-close", Duration::from_secs(5)).await,
        "opening the detail view again should focus its close button"
    );
    let mut tabbed_out = None;
    for step in 1..=20 {
        bar.press_key("Tab").await.expect("press Tab");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let inside: bool = keyboard_page
            .evaluate("(() => { const o = document.querySelector('.context-detail-overlay'); return !!o && o.contains(document.activeElement); })()")
            .await
            .expect("read the focus")
            .into_value()
            .expect("a bool");
        if !inside {
            let on: String = keyboard_page
                .evaluate("(document.activeElement && (document.activeElement.className || document.activeElement.tagName)) || 'nothing'")
                .await
                .expect("read the focus")
                .into_value()
                .unwrap_or_default();
            tabbed_out = Some(format!("Tab {step} moved focus to {on}"));
            break;
        }
    }
    assert!(tabbed_out.is_none(), "Tab should stay in the detail view: {}", tabbed_out.unwrap_or_default());
}

/// Waits up to `timeout` for `selector`'s element to have focus. Focus
/// moves the page makes in a spawned task land a moment after the change
/// that asked for them.
pub(super) async fn wait_for_focus(page: &chromiumoxide::Page, selector: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    let probe = format!("document.activeElement === document.querySelector({selector:?})");
    loop {
        let focused: bool = page
            .evaluate(probe.as_str())
            .await
            .ok()
            .and_then(|value| value.into_value().ok())
            .unwrap_or(false);
        if focused {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How far `.messages` is scrolled from its bottom, in pixels.
pub(super) async fn transcript_distance_from_bottom(page: &chromiumoxide::Page) -> f64 {
    page.evaluate("(() => { const m = document.querySelector('.messages'); return m ? m.scrollHeight - m.scrollTop - m.clientHeight : -1; })()")
        .await
        .expect("read the transcript's scroll position")
        .into_value()
        .expect("a number")
}

/// Makes the page's next DELETE request fail as a server error would;
/// every other request goes through.
pub(super) const FAIL_NEXT_DELETE: &str = "(() => { const real = window.fetch; let armed = true; \
     window.fetch = function (input, init) { \
       const method = ((init && init.method) || (input && input.method) || 'GET').toUpperCase(); \
       if (armed && method === 'DELETE') { armed = false; return Promise.resolve(new Response('refused by the browser tier', { status: 500 })); } \
       return real.apply(this, arguments); }; })()";

/// Scenario 31 (SME-58): each settings page's two-step delete arms on the
/// first click and deletes on the second (they share `TwoStepButton`), and
/// the MCP header inputs say what they are to a screen reader, not only in
/// a placeholder.
pub(super) async fn scenario_settings_two_step(t: &Scenario<'_>) {
    let pool = t.pool;
    let mcp = db::create_mcp_server_config(
        pool,
        &unique_id("two-step"),
        "http://127.0.0.1:9/mcp",
        &[("X-Existing".to_string(), "v".to_string())].into_iter().collect(),
        "static_headers",
        None,
        None,
    )
    .await
    .expect("create an MCP server");
    let page = t.tab(t.url(&format!("mcp-servers/{}", mcp.id))).await;
    wait_for_resource(&page, &format!("/api/mcp-servers/{}", mcp.id)).await;
    click_when_present(&page, ".mcp-add-header", Duration::from_secs(5)).await;
    wait_for_element(&page, ".mcp-header-name", Duration::from_secs(5)).await;
    let labels: Vec<String> = page
        .evaluate(
            "Array.from(document.querySelectorAll('.mcp-header-row input')).map(i => i.getAttribute('aria-label') || '')",
        )
        .await
        .expect("read the header inputs' labels")
        .into_value()
        .expect("strings");
    assert!(
        labels.len() >= 3 && labels.iter().all(|label| !label.is_empty()),
        "every header input should have an aria-label: {labels:?}"
    );
    two_step_delete(&page, ".mcp-delete").await;
    assert!(
        wait_until(|| async { matches!(db::get_mcp_server_config(pool, mcp.id).await, Ok(None)) }, Duration::from_secs(10)).await,
        "the MCP server should be deleted"
    );

    let server = db::create_language_server(
        pool,
        &crate::models::LanguageServerConfig {
            name: unique_id("two-step"),
            image: "rust:1".to_string(),
            command: "true".to_string(),
            memory_limit: "1Gi".to_string(),
            cpu_limit: "1".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create a language server");
    let page = t.tab(t.url(&format!("language-servers/{}", server.id))).await;
    wait_for_resource(&page, &format!("/api/language-servers/{}", server.id)).await;
    two_step_delete(&page, ".mcp-delete-server").await;
    assert!(
        wait_until(|| async { matches!(db::get_language_server(pool, server.id).await, Ok(None)) }, Duration::from_secs(10)).await,
        "the language server should be deleted"
    );

    let provider = db::create_inference_provider(pool, &unique_id("two-step"), "anthropic", "http://127.0.0.1:9", "api_key", "key", false, None, true)
        .await
        .expect("create a provider");
    let page = t.tab(t.url(&format!("providers/{}", provider.id))).await;
    wait_for_resource(&page, &format!("/api/providers/{}", provider.id)).await;
    two_step_delete(&page, ".mcp-delete-server").await;
    assert!(
        wait_until(|| async { matches!(db::get_inference_provider(pool, provider.id).await, Ok(None)) }, Duration::from_secs(10)).await,
        "the provider should be deleted"
    );
}

/// Clicks a two-step button at `selector` once (it must arm: `confirm`
/// class, a "Confirm" label) and again (to confirm).
pub(super) async fn two_step_delete(page: &chromiumoxide::Page, selector: &str) {
    wait_for_stable(page, selector, tokio::time::Instant::now() + Duration::from_secs(10)).await;
    click_when_present(page, selector, Duration::from_secs(5)).await;
    let armed = format!("{selector}.confirm");
    wait_for_element(page, &armed, Duration::from_secs(5)).await;
    let label: String = page
        .evaluate(format!("document.querySelector({armed:?}).innerText"))
        .await
        .expect("read the armed label")
        .into_value()
        .expect("text");
    assert!(label.starts_with("Confirm"), "an armed button should ask to confirm: {label:?}");
    click_when_present(page, &armed, Duration::from_secs(5)).await;
}

/// Polls `check` every 200ms until it holds or `timeout` passes.
pub(super) async fn wait_until<F, Fut>(check: F, timeout: Duration) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Scenario 30 (SME-81): an error says what went wrong, not dioxus's
/// "error running server function: … (details: None)" wrapper, and the
/// sidebar's error goes once a later sidebar action succeeds.
pub(super) async fn scenario_error_text(t: &Scenario<'_>) {
    // Through the list, whose own fetch shows the page is hydrated: typed
    // into the server-rendered form before that, the text never reaches
    // the form's state.
    let volume_page = t.tab(t.url("sandbox-volumes")).await;
    wait_for_resource(&volume_page, "/api/sandbox-volumes").await;
    click_when_present(&volume_page, ".sandbox-volumes-new-link", Duration::from_secs(5)).await;
    let name_field = wait_for_element(&volume_page, "#sandbox-volume-new-name", Duration::from_secs(10)).await;
    name_field.focus().await.expect("focus the volume name");
    name_field.type_str("scenario-30-root").await.expect("type the volume name");
    let path_field = wait_for_element(&volume_page, "#sandbox-volume-new-mount-path", Duration::from_secs(5)).await;
    path_field.focus().await.expect("focus the mount path");
    path_field.type_str("/").await.expect("type the mount path");
    click_when_present(&volume_page, ".sandbox-volume-add-form button[type=submit]", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&volume_page, "can't be mounted over the root directory", Duration::from_secs(10)).await,
        "the volume form should say why `/` was refused"
    );
    let shown: String = volume_page
        .evaluate("document.querySelector('.sandbox-volume-add-form .error').innerText")
        .await
        .expect("read the error")
        .into_value()
        .expect("text");
    assert!(
        !shown.contains("error running server function") && !shown.contains("details:"),
        "the error should be the server's message alone: {shown:?}"
    );

    // Two conversations to delete from the sidebar: the first delete fails,
    // the second succeeds. A success that navigates (New conversation)
    // remounts the sidebar and hides the bug; deleting a row that isn't
    // open stays on the page.
    let refused = t.conversation().await;
    let deleted = t.conversation().await;
    let watched = t.conversation().await;
    let sidebar = t.tab(t.url(&format!("conversation/{}", watched.id))).await;
    wait_for_live_client(&sidebar, watched.id).await;
    let delete_button = |id: i64| format!(".conversation-item[data-conversation-id='{id}'] .delete-conversation");
    wait_for_stable(&sidebar, &delete_button(refused.id), tokio::time::Instant::now() + Duration::from_secs(10)).await;
    // The next DELETE (the first conversation delete) fails as a server
    // error would; everything else goes through.
    sidebar.evaluate(FAIL_NEXT_DELETE).await.expect("make the next DELETE fail");
    click_when_present(&sidebar, &delete_button(refused.id), Duration::from_secs(5)).await;
    click_when_present(&sidebar, &format!("{}.confirm", delete_button(refused.id)), Duration::from_secs(5)).await;
    wait_for_element(&sidebar, ".sidebar-body > .error", Duration::from_secs(10)).await;
    // A message arriving in the open conversation reloads the list (as a
    // running turn's every reply does); that reload succeeding isn't the
    // failed Delete succeeding, so the error stays (SME-81 review).
    let lists_before = list_requests(&sidebar).await;
    let arrived = db::create_message(
        t.pool,
        watched.id,
        "assistant",
        &[anthropic::ContentBlock::Text { text: "scenario 30 reload".to_string() }],
    )
    .await
    .expect("save a message");
    crate::events::publish(watched.id, crate::events::ConversationEvent::MessagesAppended { messages: vec![arrived] });
    assert!(
        wait_for_text(&sidebar, "scenario 30 reload", Duration::from_secs(10)).await,
        "the open conversation should show the new message"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while list_requests(&sidebar).await <= lists_before {
        assert!(tokio::time::Instant::now() < deadline, "the new message should reload the conversation list");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The reload's result is applied by an effect after the request ends.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let kept: bool = sidebar
        .evaluate("!!document.querySelector('.sidebar-body > .error')")
        .await
        .expect("look for the sidebar error")
        .into_value()
        .expect("bool");
    assert!(kept, "a list reload shouldn't clear a failed Delete's error");
    click_when_present(&sidebar, &delete_button(deleted.id), Duration::from_secs(5)).await;
    click_when_present(&sidebar, &format!("{}.confirm", delete_button(deleted.id)), Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&sidebar, &format!(".conversation-item[data-conversation-id='{}']", deleted.id), 0, Duration::from_secs(10)).await,
        "the second delete should go through"
    );
    let still_shown: bool = sidebar
        .evaluate("!!document.querySelector('.sidebar-body > .error')")
        .await
        .expect("look for the sidebar error")
        .into_value()
        .expect("bool");
    assert!(!still_shown, "a later success should clear the sidebar's error");

    // The same on a settings page: a failed volume delete's error goes once
    // another delete succeeds (SME-81 review 2).
    let refused_volume = db::create_sandbox_volume(t.pool, &unique_id("refused"), "/home/sandbox/refused")
        .await
        .expect("create a volume")
        .id;
    let deleted_volume = db::create_sandbox_volume(t.pool, &unique_id("deleted"), "/home/sandbox/deleted")
        .await
        .expect("create a volume")
        .id;
    let volumes = t.tab(t.url("sandbox-volumes")).await;
    wait_for_resource(&volumes, "/api/sandbox-volumes").await;
    let volume_delete = |id: i64| format!(".sandbox-volume-row[data-volume-id='{id}'] .sandbox-volume-delete");
    wait_for_stable(&volumes, &volume_delete(refused_volume), tokio::time::Instant::now() + Duration::from_secs(10)).await;
    volumes.evaluate(FAIL_NEXT_DELETE).await.expect("make the next DELETE fail");
    click_when_present(&volumes, &volume_delete(refused_volume), Duration::from_secs(5)).await;
    click_when_present(&volumes, &format!("{}.confirm", volume_delete(refused_volume)), Duration::from_secs(5)).await;
    wait_for_element(&volumes, ".sandbox-volumes-page .error", Duration::from_secs(10)).await;
    click_when_present(&volumes, &volume_delete(deleted_volume), Duration::from_secs(5)).await;
    click_when_present(&volumes, &format!("{}.confirm", volume_delete(deleted_volume)), Duration::from_secs(5)).await;
    assert!(
        wait_for_count(&volumes, &format!(".sandbox-volume-row[data-volume-id='{deleted_volume}']"), 0, Duration::from_secs(10)).await,
        "the second volume delete should go through"
    );
    let still_shown: bool = volumes
        .evaluate("!!document.querySelector('.sandbox-volumes-page .error')")
        .await
        .expect("look for the volume error")
        .into_value()
        .expect("bool");
    let _ = db::delete_sandbox_volume(t.pool, refused_volume).await;
    assert!(!still_shown, "a later volume delete succeeding should clear the failed one's error");
}
