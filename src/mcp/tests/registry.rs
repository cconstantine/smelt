//! SME-167: the registry only keeps a connection built from the server's
//! row as it was after its last `evict`. Each test pins one interleaving
//! with `gated_http`'s gate: a connect (or a re-list) is held at the mock
//! while the test edits, deletes or evicts the server, then let through.

use super::gated_http::{self, HEADER, WAIT};
use super::*;

/// Saves `url` and `headers` over `config`'s row and evicts it, as
/// `update_mcp_server` does.
async fn edit(pool: &sqlx::PgPool, config: &McpServerConfig, url: &str, headers: &[(&str, &str)]) -> McpServerConfig {
    let upsert = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let edited = crate::db::update_mcp_server_config(pool, config.id, &config.name, url, &upsert, &[], "static_headers")
        .await
        .expect("update the row")
        .expect("the row exists");
    evict(config.id).await;
    edited
}

/// Starts a turn's tool list in the background, as a turn does; its
/// connect is spawned inside and outlives it.
fn list_tools_in_background(pool: &sqlx::PgPool, config: &McpServerConfig) -> tokio::task::JoinHandle<Vec<ToolDefinition>> {
    let (pool, config) = (pool.clone(), config.clone());
    tokio::spawn(async move { tool_definitions_for(&pool, &[config]).await })
}

/// Waits, at most `WAIT`, for `task` to finish.
async fn finish<T>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the task didn't finish in time")
        .expect("the task panicked")
}

/// Waits, at most `WAIT`, for the connect in flight for `server_id` to
/// end: it holds the server's `CONNECTING` lock throughout. Only called
/// once the mock holds that connect's request, so the lock is held.
async fn connect_ended(server_id: i64) {
    let lock = server_lock(&CONNECTING, server_id);
    let _ended = tokio::time::timeout(WAIT, lock.lock()).await.expect("the connect didn't end in time");
}

async fn tool_names(pool: &sqlx::PgPool, config: &McpServerConfig) -> Vec<String> {
    let listed = tokio::time::timeout(WAIT, tool_definitions_for(pool, std::slice::from_ref(config)))
        .await
        .expect("listing tools didn't finish in time");
    listed.into_iter().map(|tool| tool.name).collect()
}

fn named(config: &McpServerConfig, tool: &str) -> Vec<String> {
    vec![namespaced_tool_name(&config.name, tool)]
}

async fn connected(server_id: i64) -> bool {
    REGISTRY.lock().await.contains_key(&server_id)
}

/// The bug as reported: a connect a turn started before a URL edit
/// finished after it and was kept, so every later turn used the old
/// server's tools until a restart.
#[sqlx::test]
async fn test_a_connect_finishing_after_a_url_edit_is_not_kept(pool: sqlx::PgPool) {
    let (a, b) = (gated_http::spawn("tool_a").await, gated_http::spawn("tool_b").await);
    let config = create_static_server(&pool, &a.url).await;
    let mut held = a.hold("initialize");
    let turn = list_tools_in_background(&pool, &config);
    held.reached().await;

    let edited = edit(&pool, &config, &b.url, &[]).await;
    held.release();
    connect_ended(config.id).await;
    finish(turn).await;

    assert!(!connected(config.id).await, "the connection to the old URL was kept");
    assert_eq!(tool_names(&pool, &edited).await, named(&edited, "tool_b"));
    evict(config.id).await;
}

/// The same with only the headers changed: the next connect sends the
/// new header.
#[sqlx::test]
async fn test_a_connect_finishing_after_a_header_edit_is_not_kept(pool: sqlx::PgPool) {
    let a = gated_http::spawn("tool_a").await;
    let created = create_static_server(&pool, &a.url).await;
    let config = edit(&pool, &created, &a.url, &[(HEADER, "old")]).await;
    let mut held = a.hold("initialize");
    let turn = list_tools_in_background(&pool, &config);
    held.reached().await;

    let edited = edit(&pool, &config, &a.url, &[(HEADER, "new")]).await;
    held.release();
    connect_ended(config.id).await;
    finish(turn).await;

    assert!(!connected(config.id).await, "the connection with the old header was kept");
    assert_eq!(tool_names(&pool, &edited).await, named(&edited, "tool_a"));
    assert_eq!(
        a.initialize_headers(),
        vec![Some("old".to_string()), Some("new".to_string())],
        "the next connect should send the new header"
    );
    evict(config.id).await;
}

/// A server deleted while a connect to it was in flight stays gone, and
/// a tool call for it never reaches the old server.
#[sqlx::test]
async fn test_a_connect_finishing_after_a_delete_is_not_kept(pool: sqlx::PgPool) {
    let a = gated_http::spawn("tool_a").await;
    let config = create_static_server(&pool, &a.url).await;
    let mut held = a.hold("initialize");
    let turn = list_tools_in_background(&pool, &config);
    held.reached().await;

    crate::db::delete_mcp_server_config(&pool, config.id).await.expect("delete the row");
    evict(config.id).await;
    held.release();
    connect_ended(config.id).await;
    finish(turn).await;

    assert!(!connected(config.id).await, "the deleted server's connection was kept");
    let called = tokio::time::timeout(WAIT, call_tool(&pool, &config, "tool_a", serde_json::json!({})))
        .await
        .expect("the call didn't finish in time");
    assert!(called.as_ref().is_err_and(|e| e.contains("no longer exists")), "{called:?}");
    assert!(!connected(config.id).await);
    evict(config.id).await;
}

/// A connect to the old URL that fails after an edit isn't the new
/// server failing: the next turn tries the new URL rather than skipping
/// the server for `RETRY_AFTER_FAILURE`.
#[sqlx::test]
async fn test_a_connect_failing_after_an_edit_is_not_recorded_as_a_failure(pool: sqlx::PgPool) {
    let (a, b) = (gated_http::spawn("tool_a").await, gated_http::spawn("tool_b").await);
    let config = create_static_server(&pool, &a.url).await;
    let mut held = a.hold("initialize");
    let turn = list_tools_in_background(&pool, &config);
    held.reached().await;

    let edited = edit(&pool, &config, &b.url, &[]).await;
    held.fail();
    connect_ended(config.id).await;
    finish(turn).await;

    assert!(!failed_recently(config.id), "the old URL's failure was recorded against the server");
    assert_eq!(tool_names(&pool, &edited).await, named(&edited, "tool_b"));
    evict(config.id).await;
}

/// A caller holding the config it read before an edit (a status check,
/// or a caller queued behind the interrupted connect) connects to the
/// row as it is now, not to its copy.
#[sqlx::test]
async fn test_a_caller_holding_the_config_from_before_an_edit_connects_to_the_new_one(pool: sqlx::PgPool) {
    let (a, b) = (gated_http::spawn("tool_a").await, gated_http::spawn("tool_b").await);
    let config = create_static_server(&pool, &a.url).await;
    edit(&pool, &config, &b.url, &[]).await;

    let checked = tokio::time::timeout(WAIT, connection_check(&pool, &config))
        .await
        .expect("the check didn't finish in time");

    assert_eq!(checked, Ok(vec!["tool_b".to_string()]));
    evict(config.id).await;
}

/// A `tools/list_changed` re-list on a connection that was evicted and
/// replaced meanwhile wrote the old server's tools onto the new one.
#[sqlx::test]
async fn test_a_relist_finishing_after_an_evict_does_not_overwrite_the_new_connection(pool: sqlx::PgPool) {
    let (a, b) = (gated_http::spawn("tool_a").await, gated_http::spawn("tool_b").await);
    let config = create_static_server(&pool, &a.url).await;
    assert_eq!(connection_check(&pool, &config).await, Ok(vec!["tool_a".to_string()]));
    REGISTRY.lock().await.get(&config.id).expect("connected").stale.store(true, Ordering::SeqCst);
    let mut held = a.hold("tools/list");
    let relist = tokio::spawn({
        let (pool, config) = (pool.clone(), config.clone());
        async move { ensure_connected(&pool, &config, Attempt::Always).await.map_err(|e| e.message) }
    });
    held.reached().await;

    let edited = edit(&pool, &config, &b.url, &[]).await;
    assert_eq!(connection_check(&pool, &edited).await, Ok(vec!["tool_b".to_string()]));
    held.release();
    let relisted = finish(relist).await;

    assert!(relisted.is_ok(), "{relisted:?}");
    assert_eq!(
        connection_check(&pool, &edited).await,
        Ok(vec!["tool_b".to_string()]),
        "the old connection's re-list overwrote the new connection's tools"
    );
    evict(config.id).await;
}

/// Guards the check against refusing too much: a connect no evict of
/// its own server interrupted is kept (another server's evict doesn't
/// count), and after an interrupted one, the next connect is kept.
#[sqlx::test]
async fn test_a_connect_the_evict_did_not_interrupt_is_kept(pool: sqlx::PgPool) {
    let a = gated_http::spawn("tool_a").await;
    let config = create_static_server(&pool, &a.url).await;
    let other = create_static_server(&pool, &a.url).await;
    let mut held = a.hold("initialize");
    let turn = list_tools_in_background(&pool, &config);
    held.reached().await;
    evict(other.id).await;
    held.release();
    connect_ended(config.id).await;
    finish(turn).await;
    assert!(connected(config.id).await, "a connect no evict of its own server interrupted was refused");
    assert_eq!(tool_names(&pool, &config).await, named(&config, "tool_a"));

    evict(config.id).await;
    let mut held = a.hold("initialize");
    let turn = list_tools_in_background(&pool, &config);
    held.reached().await;
    evict(config.id).await;
    held.release();
    connect_ended(config.id).await;
    finish(turn).await;
    assert!(!connected(config.id).await, "an interrupted connect was kept");
    assert_eq!(tool_names(&pool, &config).await, named(&config, "tool_a"));
    assert!(connected(config.id).await, "the connect after it wasn't kept");
    assert_eq!(a.initialize_headers().len(), 3, "one initialize per connect");
    evict(config.id).await;
}
