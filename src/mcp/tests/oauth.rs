use super::*;

#[test]
fn test_an_oauth_server_refuses_an_authorization_header_and_takes_others() {
    let headers = |name: &str| HashMap::from([(name.to_string(), "v".to_string())]);
    for name in ["Authorization", "authorization"] {
        let refused = check_extra_headers("oauth", &headers(name));
        assert!(refused.as_ref().is_err_and(|e| e.contains("OAuth")), "{name}: {refused:?}");
    }
    assert_eq!(check_extra_headers("oauth", &headers("X-MCP-Toolsets")), Ok(()));
    assert_eq!(check_extra_headers("static_headers", &headers("Authorization")), Ok(()));
}

/// SME-113, the bug as reported: a cached connection kept the token it
/// was opened with, so once that token expired the status still said
/// Connected and every tool call failed with a 401.
#[sqlx::test]
async fn test_a_tool_call_after_the_token_expires_uses_a_refreshed_token(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    let first = oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    let config = oauth_http::config(&pool, server.id).await;
    assert_eq!(
        connection_check(&pool, &config).await.expect("connects with the first token"),
        vec!["echo".to_string()]
    );

    mock.revoke(&first);
    oauth_http::age_token(&pool, server.id).await;
    let result = call_tool(&pool, &config, "echo", serde_json::json!({})).await;

    assert_eq!(result, Ok("called with access-2".to_string()));
    assert_eq!(mock.refresh_count(), 1);
    evict(server.id).await;
}

/// SME-177 review 1: the refresh's compare-and-set is against the grant
/// as Postgres wrote it. A provider can send any extra field in its token
/// response, and serde_json (without `float_roundtrip`) doesn't parse and
/// re-serialise every 17-digit float to the same number, so comparing
/// against a re-serialised `Value` refused every refresh of such a grant.
#[sqlx::test]
async fn test_a_grant_holding_a_float_serde_json_does_not_round_trip_still_refreshes(
    pool: sqlx::PgPool,
) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    sqlx::query(
        "UPDATE mcp_servers SET oauth_credentials = jsonb_set(oauth_credentials, \
             '{token_response,issued_at}', '55281517.519135030'::jsonb) WHERE id = $1",
    )
    .bind(server.id)
    .execute(&pool)
    .await
    .expect("a provider's extra float field");
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;

    let result = call_tool(&pool, &config, "echo", serde_json::json!({})).await;

    assert_eq!(result, Ok("called with access-2".to_string()), "the refresh was saved");
    assert_eq!(mock.refresh_count(), 1);
    evict(server.id).await;
}

/// SME-113: a token the provider stopped accepting before its expiry
/// (revoked, or a clock out by more than the refresh margin) is
/// refreshed once and the request retried.
#[sqlx::test]
async fn test_a_rejected_token_that_has_not_expired_is_refreshed_and_retried(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    let first = oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    let config = oauth_http::config(&pool, server.id).await;
    connection_check(&pool, &config).await.expect("connects with the first token");

    mock.revoke(&first);
    let result = call_tool(&pool, &config, "echo", serde_json::json!({})).await;

    assert_eq!(result, Ok("called with access-2".to_string()));
    assert_eq!(mock.refresh_count(), 1, "exactly one refresh");
    evict(server.id).await;
}

/// SME-113: a refreshed grant is saved, so after a restart (nothing in
/// memory) the next connect uses it rather than refreshing again with
/// a refresh token the provider has already retired.
#[sqlx::test]
async fn test_a_refreshed_grant_survives_a_restart_without_another_refresh(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;
    connection_check(&pool, &config).await.expect("connects after a refresh");
    assert_eq!(mock.refresh_count(), 1);

    evict(server.id).await;
    let result = call_tool(&pool, &config, "echo", serde_json::json!({})).await;

    assert_eq!(result, Ok("called with access-2".to_string()));
    assert_eq!(mock.refresh_count(), 1, "the saved token was used, not refreshed again");
    evict(server.id).await;
}

/// SME-113, GitHub's case: a pre-registered client's refresh must
/// authenticate with its client secret. The code exchange sent it, but
/// a manager rebuilt from the stored grant knew only the client id, so
/// GitHub refused every refresh and the server stayed Unreachable
/// until a Reconnect.
#[sqlx::test]
async fn test_a_preregistered_client_refreshes_with_its_client_secret(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    *mock.required_secret.lock().unwrap() =
        Some(("gh-client".to_string(), "gh-secret".to_string()));
    let server = oauth_http::create_server(&pool, &mock, Some(("gh-client", "gh-secret"))).await;
    oauth_http::save_grant(&pool, server.id, &mock, "gh-client").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;

    let result = call_tool(&pool, &config, "echo", serde_json::json!({})).await;

    assert_eq!(result, Ok("called with access-2".to_string()));
    assert_eq!(mock.refresh_count(), 1);
    evict(server.id).await;
}

/// SME-113: a call that fails below MCP (here a 401 that survives the
/// refresh-and-retry) drops the connection, so the next caller connects
/// again instead of every call failing on the same dead connection. A
/// JSON-RPC error from the tool itself keeps it.
#[sqlx::test]
async fn test_a_tool_call_that_fails_at_the_transport_drops_the_connection(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    let first = oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    let config = oauth_http::config(&pool, server.id).await;
    connection_check(&pool, &config).await.expect("connects");

    let refused = call_tool(&pool, &config, "fail", serde_json::json!({})).await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(REGISTRY.lock().await.contains_key(&server.id), "a tool's own error keeps the connection");

    mock.revoke(&first);
    mock.reject_refreshes.store(true, Ordering::SeqCst);
    let failed = call_tool(&pool, &config, "echo", serde_json::json!({})).await;
    assert!(failed.is_err(), "{failed:?}");
    assert!(!REGISTRY.lock().await.contains_key(&server.id), "the dead connection is dropped");

    mock.reject_refreshes.store(false, Ordering::SeqCst);
    connection_check(&pool, &config).await.expect("the next check connects again");
    assert_eq!(mock.initialize_count(), 2, "a fresh connection, not the cached one");
    evict(server.id).await;
}

/// Starts a tool call whose connect has to refresh the (aged) token,
/// and returns once the refresh has reached the provider, where it
/// waits for the returned gate.
async fn call_held_at_refresh(
    pool: &sqlx::PgPool,
    mock: &Arc<oauth_http::Mock>,
    config: &McpServerConfig,
) -> (tokio::task::JoinHandle<Result<String, String>>, Arc<tokio::sync::Semaphore>) {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *mock.refresh_gate.lock().unwrap() = Some(gate.clone());
    let call = tokio::spawn({
        let (pool, config) = (pool.clone(), config.clone());
        async move { call_tool(&pool, &config, "echo", serde_json::json!({})).await }
    });
    assert!(
        wait_until(|| !mock.refresh_client_ids.lock().unwrap().is_empty()).await,
        "the refresh should reach the provider"
    );
    (call, gate)
}

async fn wait_until(check: impl Fn() -> bool) -> bool {
    for _ in 0..50 {
        if check() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

async fn stored_client_id(pool: &sqlx::PgPool, server_id: i64) -> Option<String> {
    oauth_http::config(pool, server_id)
        .await
        .oauth_credentials
        .and_then(|json| json.0.get("client_id")?.as_str().map(str::to_string))
}

/// SME-113: a refresh that finishes after a Disconnect doesn't write
/// the grant back (the server would quietly reconnect itself), and the
/// call it was for fails rather than going on with it.
#[sqlx::test]
async fn test_a_refresh_finishing_after_a_disconnect_does_not_restore_the_grant(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;
    let (call, gate) = call_held_at_refresh(&pool, &mock, &config).await;

    crate::mcp_oauth::disconnect(&pool, &config).await.expect("disconnect");
    evict(server.id).await;
    gate.add_permits(1);
    let result = call.await.expect("call task");

    assert!(result.is_err(), "the call went on with a removed grant: {result:?}");
    assert_eq!(oauth_http::config(&pool, server.id).await.oauth_credentials, None);
    assert!(!REGISTRY.lock().await.contains_key(&server.id), "nothing cached for a disconnected server");
    evict(server.id).await;
}

/// SME-113: nor does it overwrite a new grant a Connect saved meanwhile
/// (dynamic registration gives each Connect a new client id).
#[sqlx::test]
async fn test_a_refresh_finishing_after_a_new_grant_keeps_the_new_grant(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;
    let (call, gate) = call_held_at_refresh(&pool, &mock, &config).await;

    oauth_http::save_grant(&pool, server.id, &mock, "client-1").await;
    evict(server.id).await;
    gate.add_permits(1);
    let _ = call.await.expect("call task");

    assert_eq!(stored_client_id(&pool, server.id).await.as_deref(), Some("client-1"));
    evict(server.id).await;
}

/// SME-177 B24: a Reconnect with a pre-registered client keeps the client
/// id, so the client-id check alone let a refresh of the old grant,
/// finishing after the Reconnect, overwrite the new grant.
#[sqlx::test]
async fn test_a_refresh_finishing_after_a_same_client_reconnect_keeps_the_new_grant(
    pool: sqlx::PgPool,
) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;
    let (call, gate) = call_held_at_refresh(&pool, &mock, &config).await;

    let reconnected = oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    evict(server.id).await;
    gate.add_permits(1);
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), call)
        .await
        .expect("the call finishes once the refresh is let through")
        .expect("call task");

    let stored = oauth_http::config(&pool, server.id).await.oauth_credentials.expect("a grant");
    assert_eq!(
        stored.0["token_response"]["access_token"].as_str(),
        Some(reconnected.as_str()),
        "the Reconnect's grant stays"
    );
    assert!(result.is_err(), "the call went on with the replaced grant: {result:?}");
    evict(server.id).await;
}

/// SME-113: a grant the provider won't refresh any more shows as such,
/// not as Connected from the cached connection, and the connection is
/// dropped.
#[sqlx::test]
async fn test_a_sign_in_that_cannot_be_refreshed_is_reported_as_expired(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    let config = oauth_http::config(&pool, server.id).await;
    connection_check(&pool, &config).await.expect("connects");

    oauth_http::age_token(&pool, server.id).await;
    mock.reject_refreshes.store(true, Ordering::SeqCst);
    let checked = connection_check(&pool, &config).await;

    assert!(
        matches!(&checked, Err(CheckError::SignInExpired(message)) if message.contains("Reconnect")),
        "{checked:?}"
    );
    assert!(!REGISTRY.lock().await.contains_key(&server.id), "the dead connection is dropped");
    evict(server.id).await;
}

/// SME-113: the status check and a connection refresh through one
/// manager per server. With a manager each, both refreshed with the
/// same refresh token, and the provider (rotating it, as GitHub does)
/// refused the second, reporting a live sign-in as expired.
#[sqlx::test]
async fn test_the_status_check_and_a_tool_call_share_one_refresh(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *mock.refresh_gate.lock().unwrap() = Some(gate.clone());

    let check = tokio::spawn({
        let (pool, config) = (pool.clone(), config.clone());
        async move { connection_check(&pool, &config).await }
    });
    let call = tokio::spawn({
        let (pool, config) = (pool.clone(), config.clone());
        async move { call_tool(&pool, &config, "echo", serde_json::json!({})).await }
    });
    assert!(
        wait_until(|| !mock.refresh_client_ids.lock().unwrap().is_empty()).await,
        "a refresh should reach the provider"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    gate.add_permits(2);

    let (checked, called) = (check.await.expect("check task"), call.await.expect("call task"));
    assert_eq!(checked, Ok(vec!["echo".to_string()]));
    assert_eq!(called, Ok("called with access-2".to_string()));
    assert_eq!(mock.refresh_client_ids.lock().unwrap().len(), 1, "one refresh request");
    evict(server.id).await;
}

/// SME-113: a manager built for an older grant isn't reused for a new
/// one, even when no `evict` came between them (a build that finished
/// after the callback's evict), so the new grant's refreshes name its
/// own client. Review 1: this test used to evict first, which left
/// nothing cached to reuse.
#[sqlx::test]
async fn test_a_new_grant_refreshes_with_its_own_client(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-1").await;
    let config = oauth_http::config(&pool, server.id).await;
    connection_check(&pool, &config).await.expect("connects with the first grant");
    assert!(OAUTH_CLIENTS.lock().await.contains_key(&server.id), "the first grant's manager is cached");

    oauth_http::save_grant(&pool, server.id, &mock, "client-2").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;
    connection_check(&pool, &config).await.expect("connects with the new grant");

    assert_eq!(
        mock.refresh_client_ids.lock().unwrap().last().map(String::as_str),
        Some("client-2")
    );
    evict(server.id).await;
    assert!(!OAUTH_CLIENTS.lock().await.contains_key(&server.id), "evict drops the manager");
}

/// SME-113: a token revoked before its expiry, whose refresh the
/// provider refuses, shows as an expired sign-in on a fresh connect
/// (the server's 401 after `AuthClient`'s one refresh), not as an
/// unreachable server.
#[sqlx::test]
async fn test_a_revoked_sign_in_that_cannot_be_refreshed_is_reported_as_expired(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    let first = oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    mock.revoke(&first);
    mock.reject_refreshes.store(true, Ordering::SeqCst);
    let config = oauth_http::config(&pool, server.id).await;

    let checked = connection_check(&pool, &config).await;

    assert!(matches!(&checked, Err(CheckError::SignInExpired(_))), "{checked:?}");
    assert!(
        !OAUTH_CLIENTS.lock().await.contains_key(&server.id),
        "an expired sign-in drops the server's manager too (review 1)"
    );
    evict(server.id).await;
}

/// SME-113 review 1: GitHub refuses a dead refresh token with HTTP 200
/// and `error=bad_refresh_token`, not a 400 `invalid_grant`, so rmcp
/// took it for a failed request and the server read Unreachable, with
/// Connect, instead of Sign-in expired.
#[sqlx::test]
async fn test_a_refresh_github_refuses_is_reported_as_expired(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    oauth_http::age_token(&pool, server.id).await;
    mock.reject_refreshes_like_github.store(true, Ordering::SeqCst);
    let config = oauth_http::config(&pool, server.id).await;

    let checked = connection_check(&pool, &config).await;

    assert!(matches!(&checked, Err(CheckError::SignInExpired(_))), "{checked:?}");
    evict(server.id).await;
}

/// SME-113 review 2: a status check that gives up while its refresh is
/// at the provider mustn't drop the reply. GitHub has rotated the
/// refresh token by then, so an unsaved reply loses the sign-in.
#[sqlx::test]
async fn test_a_status_check_that_times_out_mid_refresh_keeps_the_new_grant(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    oauth_http::age_token(&pool, server.id).await;
    let config = oauth_http::config(&pool, server.id).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *mock.refresh_gate.lock().unwrap() = Some(gate.clone());

    let checked = connection_check(&pool, &config).await;
    assert!(matches!(&checked, Err(CheckError::Unreachable(_))), "the check gives up: {checked:?}");
    gate.add_permits(1);
    assert!(wait_until(|| mock.refresh_count() == 1).await, "the provider rotates the token");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    *mock.refresh_gate.lock().unwrap() = None;

    let result = call_tool(&pool, &config, "echo", serde_json::json!({})).await;
    assert_eq!(result, Ok("called with access-2".to_string()), "the rotated grant was saved");
    evict(server.id).await;
}

/// SME-113 review 2: after an `evict` mid-call, an old manager and the
/// status check's new one can refresh with the same refresh token. The
/// provider refuses whichever comes second, but the other just saved a
/// good grant, so the check mustn't call the sign-in expired.
#[sqlx::test]
async fn test_a_check_that_loses_a_refresh_race_is_not_reported_as_expired(pool: sqlx::PgPool) {
    let mock = oauth_http::spawn().await;
    let server = oauth_http::create_server(&pool, &mock, None).await;
    oauth_http::save_grant(&pool, server.id, &mock, "client-0").await;
    let config = oauth_http::config(&pool, server.id).await;
    connection_check(&pool, &config).await.expect("connects");
    let old_client = OAUTH_CLIENTS.lock().await.get(&server.id).expect("cached").client.clone();
    evict(server.id).await;
    oauth_http::age_token(&pool, server.id).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *mock.refresh_gate.lock().unwrap() = Some(gate.clone());

    let old_refresh = tokio::spawn(async move { old_client.get_access_token().await });
    assert!(wait_until(|| mock.refresh_client_ids.lock().unwrap().len() == 1).await);
    let check = tokio::spawn({
        let (pool, config) = (pool.clone(), config.clone());
        async move { connection_check(&pool, &config).await }
    });
    assert!(wait_until(|| mock.refresh_client_ids.lock().unwrap().len() == 2).await);
    gate.add_permits(1);
    old_refresh.await.expect("old refresh task").expect("the first refresh wins");
    gate.add_permits(1);

    assert_eq!(check.await.expect("check task"), Ok(vec!["echo".to_string()]));
    evict(server.id).await;
}
