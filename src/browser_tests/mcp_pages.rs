use super::*;

/// SME-113: an OAuth server whose sign-in can't be refreshed (here an
/// expired token with no refresh token) shows "Sign-in expired", its own
/// status, on the list and the edit page, which offers Reconnect and
/// Disconnect. Before, it read Connected until a tool call failed.
pub(super) async fn scenario_oauth_sign_in_expired(t: &Scenario<'_>) {
    let pool = t.pool;
    // The provider: nothing to discover (rmcp falls back to its default
    // endpoints), and no refresh is ever attempted without a refresh token.
    // Its `/mcp`, the first request of each manager build's discovery,
    // takes 1.2 s: past the test build's 1 s connect budget, which an
    // OAuth server made inline ran on, so the list read Unreachable on a
    // loaded runner (SME-501). The delay makes that crossing every run's.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the provider");
    let addr = listener.local_addr().expect("provider address");
    let slow_probe = axum::Router::new().route(
        "/mcp",
        axum::routing::any(|| async {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            axum::http::StatusCode::NOT_FOUND
        }),
    );
    let provider = tokio::spawn(async move {
        let _ = axum::serve(listener, slow_probe).await;
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
    // With the title, the check's own error text, so a red names its branch.
    let badge_text = format!(
        "(() => {{ const b = document.querySelector({badge_selector:?}); return b ? b.className + '|' + b.innerText + '|' + (b.title || '') : ''; }})()"
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
        badge.contains("mcp-status-needs-reconnect") && badge.contains("|Sign-in expired|"),
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
