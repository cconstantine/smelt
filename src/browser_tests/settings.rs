use super::*;

/// SME-51 B11: switching conversations resets what belonged to the one
/// left: its context detail view doesn't stay open over the next
/// conversation.
pub(super) async fn scenario_switch_resets_context_detail(t: &Scenario<'_>) {
    let leaving = t.conversation().await;
    let arriving = t.conversation().await;
    seed_usage(t.pool, leaving.id, 1_000, 100).await;
    let switching = t.tab(t.url(&format!("conversation/{}", leaving.id))).await;
    click_when_present(&switching, ".context-usage-bar", Duration::from_secs(10)).await;
    assert!(
        wait_for_count(&switching, ".context-detail-overlay", 1, Duration::from_secs(5)).await,
        "the context detail should open"
    );
    click_conversation(&switching, arriving.id).await;
    assert!(
        wait_for_count(&switching, ".context-detail-overlay", 0, Duration::from_secs(5)).await,
        "the first conversation's context detail stayed open after switching"
    );
}

/// Scenario 24 (SME-72): the model picker above the message box says which
/// model the conversation runs on, and choosing another in one tab shows in
/// every tab on it, with no reload.
pub(super) async fn scenario_model_picker(t: &Scenario<'_>) {
    let pool = t.pool;
    let picking = t.conversation().await;
    let provider_name = db::get_inference_provider(pool, *MOCK_PROVIDER.get().expect("the mock provider"))
        .await
        .expect("read the mock provider")
        .expect("the mock provider exists")
        .name;
    let first_tab = t.tab(t.url(&format!("conversation/{}", picking.id))).await;
    let second_tab = t.tab(t.url(&format!("conversation/{}", picking.id))).await;
    wait_for_live_client(&first_tab, picking.id).await;
    wait_for_live_client(&second_tab, picking.id).await;
    let current = format!("{provider_name} \u{b7} {MOCK_MODEL}");
    for tab in [&first_tab, &second_tab] {
        assert!(
            wait_for_text(tab, &current, Duration::from_secs(10)).await,
            "the picker should show the conversation's model, {current:?}"
        );
    }
    click_when_present(&first_tab, ".model-picker-button", Duration::from_secs(5)).await;
    // Clear the field and type, until it holds exactly the new id: a
    // re-render between selecting the old text and typing would
    // otherwise leave the two run together.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let model_field = wait_for_element(&first_tab, ".model-chooser-model", Duration::from_secs(5)).await;
        model_field.focus().await.expect("focus the model field");
        first_tab
            .evaluate("document.querySelector('.model-chooser-model').select()")
            .await
            .expect("select the current model id");
        model_field.type_str("other-model").await.expect("type a model id");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let typed: String = first_tab
            .evaluate("document.querySelector('.model-chooser-model')?.value ?? ''")
            .await
            .expect("read the model field")
            .into_value()
            .expect("a string");
        if typed == "other-model" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the model field never held just the new id: {typed:?}");
    }
    click_when_present(&first_tab, ".model-chooser button[type=\"submit\"]", Duration::from_secs(5)).await;
    let chosen = format!("{provider_name} \u{b7} other-model");
    if !wait_for_text(&first_tab, &chosen, Duration::from_secs(10)).await {
        let shown: String = first_tab
            .evaluate("document.querySelector('.model-picker')?.innerText ?? ''")
            .await
            .expect("read the picker")
            .into_value()
            .expect("a string");
        panic!("the choosing tab should show the new model; the picker says {shown:?}");
    }
    assert!(
        wait_for_text(&second_tab, &chosen, Duration::from_secs(10)).await,
        "the other tab should show the new model live"
    );
    let stored = db::get_conversation_model(pool, picking.id)
        .await
        .expect("read the conversation's model")
        .expect("the conversation exists");
    assert_eq!(stored.model.as_deref(), Some("other-model"));
}

/// SME-113: an OAuth server whose sign-in can't be refreshed (here an
/// expired token with no refresh token) shows "Sign-in expired", its own
/// status, on the list and the edit page, which offers Reconnect and
/// Disconnect. Before, it read Connected until a tool call failed.
pub(super) async fn scenario_oauth_sign_in_expired(t: &Scenario<'_>) {
    let pool = t.pool;
    // The provider: nothing to discover (rmcp falls back to its default
    // endpoints), and no refresh is ever attempted without a refresh token.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the provider");
    let addr = listener.local_addr().expect("provider address");
    let provider = tokio::spawn(async move {
        let _ = axum::serve(listener, axum::Router::new()).await;
    });
    let server = db::create_mcp_server_config(
        pool,
        &unique_id("sign-in-expired"),
        &format!("http://{addr}/mcp"),
        &std::collections::HashMap::new(),
        "oauth",
        None,
        None,
    )
    .await
    .expect("create an OAuth server");
    db::set_mcp_server_oauth_credentials(
        pool,
        server.id,
        Some(serde_json::json!({
            "client_id": "client",
            "token_response": {"access_token": "expired", "token_type": "bearer", "expires_in": 60},
            "granted_scopes": [],
            "token_received_at": 1_000_000_000u64,
        })),
    )
    .await
    .expect("store an expired grant");

    let list = t.tab(t.url("mcp-servers")).await;
    let badge_selector = format!("a[href='/mcp-servers/{}'] .mcp-status", server.id);
    let badge_text = format!(
        "(() => {{ const b = document.querySelector({badge_selector:?}); return b ? b.className + '|' + b.innerText : ''; }})()"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let badge = loop {
        let badge: String = list
            .evaluate(badge_text.as_str())
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .unwrap_or_default();
        if !badge.contains("checking") && !badge.is_empty() || tokio::time::Instant::now() >= deadline {
            break badge;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let edit = t.tab(t.url(&format!("mcp-servers/{}", server.id))).await;
    let status_shown = wait_for_text(&edit, "Sign-in expired \u{2014} use Reconnect below.", Duration::from_secs(15)).await;
    let buttons: Vec<String> = edit
        .evaluate("Array.from(document.querySelectorAll('.mcp-oauth-actions button')).map(b => b.innerText)")
        .await
        .ok()
        .and_then(|v| v.into_value().ok())
        .unwrap_or_default();

    // Deleted before the checks, so a failing one doesn't leave it behind.
    db::delete_mcp_server_config(pool, server.id).await.expect("delete the OAuth server");
    crate::mcp::evict(server.id).await;
    provider.abort();
    assert!(
        badge.contains("mcp-status-needs-reconnect") && badge.ends_with("|Sign-in expired"),
        "the list's badge: {badge:?}"
    );
    assert!(status_shown, "the edit page should say the sign-in expired");
    assert_eq!(buttons, vec!["Reconnect".to_string(), "Disconnect".to_string()]);
}

/// Scenario 25 (SME-76): an OAuth server takes extra headers too, such as
/// GitHub's `X-MCP-Toolsets` (which turns on the tools that read CI logs).
/// The edit page only showed its header editor to static-header servers.
pub(super) async fn scenario_oauth_headers(t: &Scenario<'_>) {
    let pool = t.pool;
    let oauth_server = db::create_mcp_server_config(
        pool,
        &unique_id("oauth-headers"),
        "http://127.0.0.1:9/mcp",
        &std::collections::HashMap::new(),
        "oauth",
        Some("client-id"),
        None,
    )
    .await
    .expect("create an OAuth server");
    let edit = t.tab(t.url(&format!("mcp-servers/{}", oauth_server.id))).await;
    wait_for_element(&edit, ".mcp-oauth-panel", Duration::from_secs(10)).await;
    click_when_present(&edit, ".mcp-add-header", Duration::from_secs(5)).await;
    let name_field = wait_for_element(&edit, ".mcp-header-name", Duration::from_secs(5)).await;
    name_field.focus().await.expect("focus the header name");
    name_field.type_str("X-MCP-Toolsets").await.expect("type the header name");
    let value_field = wait_for_element(&edit, ".mcp-header-row .mcp-header-value", Duration::from_secs(5)).await;
    value_field.focus().await.expect("focus the header value");
    value_field.type_str("repos,actions").await.expect("type the header value");
    click_when_present(&edit, ".mcp-save-edit", Duration::from_secs(5)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let saved = loop {
        let config = db::get_mcp_server_config(pool, oauth_server.id)
            .await
            .expect("read the server")
            .expect("the server exists");
        if !config.extra_headers.0.is_empty() || tokio::time::Instant::now() >= deadline {
            break config;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    // Deleted before the checks, so a failing one doesn't leave it behind.
    db::delete_mcp_server_config(pool, oauth_server.id).await.expect("delete the OAuth server");
    assert_eq!(
        saved.extra_headers.0.get("X-MCP-Toolsets").map(String::as_str),
        Some("repos,actions"),
        "the header should be saved: {:?}",
        saved.extra_headers.0.keys().collect::<Vec<_>>()
    );
    assert_eq!(saved.auth_mode, "oauth", "saving headers keeps the server on OAuth");
}

/// Puts the server's build id back to its own when dropped, so a failing
/// stale-bundle scenario doesn't leave every later page asking for a
/// reload.
pub(super) struct ResetBuildId;

impl Drop for ResetBuildId {
    fn drop(&mut self) {
        crate::api::version::test_override::set(None);
    }
}

/// Scenario 26 (SME-43): a tab running an older bundle than the server.
pub(super) async fn scenario_stale_bundle(t: &Scenario<'_>) {
    let _reset = ResetBuildId;
    // (a) An event type the bundle doesn't know is skipped: the stream stays
    // up (one connection's pull, not a second after a reconnect), and the
    // next event still arrives live. It also proves the server is newer,
    // so the tab asks for a reload.
    let stale = t.conversation().await;
    let stale_url = t.url(&format!("conversation/{}", stale.id));
    let stale_tab = t.tab(stale_url.as_str()).await;
    wait_for_live_client(&stale_tab, stale.id).await;
    assert!(
        !banner_shown(&stale_tab).await,
        "a page built from the server's own tree shouldn't ask for a reload"
    );
    crate::events::publish(stale.id, crate::events::ConversationEvent::BrowserTestAddedLater {});
    crate::events::publish(
        stale.id,
        crate::events::ConversationEvent::TodoListUpdate {
            items: vec![anthropic::tools::TodoItem {
                content: "scenario 26 todo".to_string(),
                status: anthropic::tools::TodoStatus::Pending,
            }],
        },
    );
    assert!(
        wait_for_text(&stale_tab, "scenario 26 todo", Duration::from_secs(5)).await,
        "an event after one of an unknown type should still arrive live"
    );
    assert_eq!(
        live_pulls(&stale_tab, stale.id).await,
        Some(1),
        "an unknown event type shouldn't end the stream"
    );
    assert!(
        wait_for_text(&stale_tab, "smelt was updated", Duration::from_secs(5)).await,
        "an unknown event type should ask for a reload"
    );
    stale_tab.close().await.expect("close the stale tab");

    // (b) The server is redeployed: the tab's stream drops, and the
    // reconnect finds another build. The Sandboxes page, whose stream
    // is the app-wide one, notices too. Reloading clears it.
    let tab = t.tab(stale_url.as_str()).await;
    wait_for_live_client(&tab, stale.id).await;
    assert!(!banner_shown(&tab).await, "no reload banner before the server changes");
    crate::api::version::test_override::set(Some("a-newer-build"));
    crate::events::forget(stale.id);
    assert!(
        wait_for_text(&tab, "smelt was updated", Duration::from_secs(10)).await,
        "a reconnect to a newer server should ask for a reload"
    );
    let pods_tab = t.tab(t.url("pods")).await;
    assert!(
        wait_for_text(&pods_tab, "smelt was updated", Duration::from_secs(10)).await,
        "the Sandboxes page should notice a newer server too"
    );
    pods_tab.close().await.expect("close the Sandboxes page");
    crate::api::version::test_override::set(None);
    click_when_present(&tab, ".stale-bundle-banner button", Duration::from_secs(5)).await;
    assert!(
        wait_for_text_gone(&tab, "smelt was updated", Duration::from_secs(10)).await,
        "Reload should load the page again"
    );
    wait_for_live_client(&tab, stale.id).await;
    assert!(!banner_shown(&tab).await, "the reloaded page is current");
}

/// Scenario 27 (SME-83): scrolling the transcript up a little leaves it
/// there. Every scroll event set the stuck-to-bottom flag, which reran the
/// auto-scroll effect, so a scroll that stayed within the 32px slack (a
/// trackpad's first small steps) snapped back.
pub(super) async fn scenario_transcript_scroll(t: &Scenario<'_>) {
    let long_conversation = t.conversation().await;
    for i in 0..40 {
        db::create_message(
            t.pool,
            long_conversation.id,
            if i % 2 == 0 { "user" } else { "assistant" },
            &[anthropic::ContentBlock::Text { text: format!("scroll filler message {i}") }],
        )
        .await
        .expect("seed a message");
    }
    let long_page = t.tab(t.url(&format!("conversation/{}", long_conversation.id))).await;
    wait_for_live_client(&long_page, long_conversation.id).await;
    // Settled at the bottom: the same distance on two reads apart.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = f64::NAN;
    loop {
        let distance = transcript_distance_from_bottom(&long_page).await;
        if distance <= 1.0 && distance == last {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the transcript never settled at its bottom: {distance}px away"
        );
        last = distance;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    long_page
        .evaluate("(() => { const m = document.querySelector('.messages'); m.scrollTop = m.scrollTop - 10; })()")
        .await
        .expect("scroll the transcript up");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let distance = transcript_distance_from_bottom(&long_page).await;
    assert!(
        (8.0..=12.0).contains(&distance),
        "a 10px scroll up should stay where it was put, but the transcript is {distance}px from its bottom"
    );
}

/// SME-168: no text in a link shows as the edit page's sign-in error. A
/// failed callback keeps its error on the server and redirects with only
/// `?oauth_callback=1`; the page takes the text from the server, once.
pub(super) async fn scenario_oauth_callback_error_link(t: &Scenario<'_>) {
    const MARKER: &str = "Your session expired. Call support at 555-0100";
    const UNVERIFIED: &str = "smelt couldn't verify the authorization provider's response";
    const NOT_CONNECTED: &str = "Not connected \u{2014} use Connect below.";
    let pool = t.pool;
    let server = db::create_mcp_server_config(
        pool,
        &unique_id("oauth-callback-error"),
        "http://127.0.0.1:9/mcp",
        &std::collections::HashMap::new(),
        "oauth",
        None,
        None,
    )
    .await
    .expect("create an OAuth server");
    let id = server.id;
    let mut failures: Vec<String> = Vec::new();
    // The status shows first; the error, if any, comes from an effect
    // after mount, so give it a moment before saying none came.
    let settle = |page: chromiumoxide::Page| async move {
        let settled = wait_for_text(&page, NOT_CONNECTED, Duration::from_secs(15)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let text: String = page.evaluate("document.body.innerText").await.ok().and_then(|v| v.into_value().ok()).unwrap_or_default();
        let errors: usize = page
            .evaluate("document.querySelectorAll('.mcp-oauth-panel .error').length")
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .unwrap_or(usize::MAX);
        (settled, text, errors)
    };
    let encoded: String = percent_encoding::utf8_percent_encode(MARKER, percent_encoding::NON_ALPHANUMERIC).collect();

    // A link carrying its own text: nothing shows.
    let forged = t.tab(t.url(&format!("mcp-servers/{id}?oauth_error={encoded}"))).await;
    let (settled, text, errors) = settle(forged).await;
    if !settled || text.contains(MARKER) || errors != 0 {
        failures.push(format!("?oauth_error= link: settled {settled}, {errors} errors, marker shown {}", text.contains(MARKER)));
    }

    // The real route, with a forged error callback: the redirect carries
    // only the flag, the page strips it and shows smelt's own text.
    let callback = t
        .tab(t.url(&format!("oauth/mcp-callback/{id}?error=access_denied&error_description={encoded}&state=forged")))
        .await;
    let shown = wait_for_text(&callback, UNVERIFIED, Duration::from_secs(15)).await;
    let delivered: String = callback
        .evaluate("performance.getEntriesByType('navigation')[0].name")
        .await
        .ok()
        .and_then(|v| v.into_value().ok())
        .unwrap_or_default();
    let search: String = callback.evaluate("location.search").await.ok().and_then(|v| v.into_value().ok()).unwrap_or_default();
    let text: String = callback.evaluate("document.body.innerText").await.ok().and_then(|v| v.into_value().ok()).unwrap_or_default();
    if !shown || !delivered.ends_with(&format!("/mcp-servers/{id}?oauth_callback=1")) || !search.is_empty() || text.contains(MARKER) {
        failures.push(format!(
            "callback: unverified shown {shown}, delivered {delivered:?}, search {search:?}, marker shown {}",
            text.contains(MARKER)
        ));
    }
    callback.reload().await.expect("reload the edit page");
    let (settled, _, errors) = settle(callback).await;
    if !settled || errors != 0 {
        failures.push(format!("after a reload: settled {settled}, {errors} errors"));
    }

    // A bare flag with nothing kept: nothing shows.
    let flag_only = t.tab(t.url(&format!("mcp-servers/{id}?oauth_callback=1"))).await;
    let (settled, _, errors) = settle(flag_only).await;
    if !settled || errors != 0 {
        failures.push(format!("bare flag: settled {settled}, {errors} errors"));
    }

    // Deleted before the checks, so a failing one doesn't leave it behind.
    db::delete_mcp_server_config(pool, id).await.expect("delete the OAuth server");
    assert!(failures.is_empty(), "{failures:#?}");
}
