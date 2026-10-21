use super::*;

#[tokio::test]
async fn test_tool_list_changed_notification_refreshes_cached_tools() {
    let server_id = -1003;
    // `second_tool_disabled: true` — the second tool exists on the
    // server from the start but is hidden, so `enable_route` below is
    // what actually fires `tools/list_changed` (adding a brand-new
    // route via `add_route` does not notify at all — confirmed against
    // `rmcp`'s source: only `enable_route`/`disable_route` call
    // `notify_if_visible`).
    let server = register_test_connection(server_id, true).await;

    let before =
        tool_definitions_for(&test_pool(), &[test_config(server_id, "test-server")]).await;
    assert_eq!(
        before.len(),
        1,
        "starts with just the echo tool; second_tool begins disabled"
    );

    // Real behavior, not a manual eviction: nothing but the
    // notification tells tool_definitions_for the cached list is stale.
    server.router.write().await.enable_route("second_tool");

    // Give the notification a moment to propagate over the duplex
    // transport and flip the connection's `stale` flag.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let after =
        tool_definitions_for(&test_pool(), &[test_config(server_id, "test-server")]).await;
    let names: Vec<&str> = after.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        after.len(),
        2,
        "expected both tools after the list_changed notification, got: {names:?}"
    );
    assert!(names.contains(&"mcp__test-server__second_tool"));

    REGISTRY.lock().await.remove(&server_id);
}

#[tokio::test]
async fn test_connection_check_reports_tool_names_for_a_connected_server() {
    let server_id = -1004;
    register_test_connection(server_id, false).await;

    let tool_names = connection_check(&test_pool(), &test_config(server_id, "test-server"))
        .await
        .expect("a registered connection should report as connected");

    assert_eq!(tool_names, vec!["echo".to_string()]);

    REGISTRY.lock().await.remove(&server_id);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_connection_check_errors_for_an_unconfigured_server(pool: sqlx::PgPool) {
    // No `register_test_connection` call — nothing in the registry, and
    // the row's URL is deliberately unroutable, so this must report an
    // error rather than panicking or hanging.
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let result = connection_check(&pool, &config).await;
    assert!(
        result.is_err(),
        "expected an unreachable server to report Err, got {result:?}"
    );
}

#[tokio::test]
async fn test_retry_once_succeeds_after_a_single_failure() {
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let result = retry_once(|| {
        let n = attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                Err("first attempt fails".to_string())
            } else {
                Ok(42)
            }
        }
    })
    .await;

    assert_eq!(result, Ok(42));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "expected exactly one retry after the first failure"
    );
}

#[tokio::test]
async fn test_retry_once_gives_up_after_a_second_failure() {
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let result: Result<i32, String> = retry_once(|| {
        attempts.fetch_add(1, Ordering::SeqCst);
        async move { Err("still failing".to_string()) }
    })
    .await;

    assert_eq!(result, Err("still failing".to_string()));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "expected no more than one retry"
    );
}
