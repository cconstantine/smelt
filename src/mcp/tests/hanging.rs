use super::*;

/// A server that accepts connections and never answers, like a host
/// that's up but whose MCP server is wedged.
async fn start_hanging_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    format!("http://{addr}/mcp")
}

/// A server that accepts and immediately hangs up, counting how many
/// times anyone tried.
async fn start_refusing_server() -> (String, Arc<std::sync::atomic::AtomicU32>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = attempts.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (format!("http://{addr}/mcp"), attempts)
}

/// SME-40 F1: connecting to one slow server held the global registry
/// lock, so every other MCP operation (another server's tools, the
/// status page, deleting a server) waited up to a minute behind it.
#[tokio::test]
async fn test_a_hanging_server_doesnt_block_other_servers() {
    let hanging = config_at(-1101, start_hanging_server().await);
    let listing = tokio::spawn(async move {
        tool_definitions_for(&test_pool(), &[hanging]).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let other = tokio::time::timeout(std::time::Duration::from_secs(1), evict(-1102)).await;
    assert!(other.is_ok(), "evicting another server waited behind the hanging connect");
    listing.abort();
}

/// SME-51 B8: the status page's check had no time limit of its own, so
/// an unreachable server read "Checking…" until the operating system
/// gave up on the connection (143 s on the bug bash).
#[tokio::test]
async fn test_the_status_check_gives_up_on_a_hanging_server() {
    let hanging = config_at(-1111, start_hanging_server().await);
    let checked = tokio::time::timeout(
        CONNECT_TIMEOUT * 2 + std::time::Duration::from_secs(3),
        connection_check(&test_pool(), &hanging),
    )
    .await;
    assert!(checked.is_ok_and(|r| r.is_err()), "the check should fail in bounded time");
}

/// SME-51 B8: a tool call that never returns held the turn, and the
/// conversation's lock, forever.
#[tokio::test]
async fn test_a_tool_call_that_never_returns_times_out() {
    register_test_connection(-1112, false).await;
    let config = test_config(-1112, "hangs");
    let called = tokio::time::timeout(
        CALL_TIMEOUT + std::time::Duration::from_secs(3),
        call_tool(&test_pool(), &config, "hang", serde_json::json!({})),
    )
    .await;
    assert!(
        called.as_ref().is_ok_and(|r| r.as_ref().is_err_and(|e| e.contains("didn't answer"))),
        "{called:?}"
    );
    assert!(
        REGISTRY.lock().await.contains_key(&-1112),
        "a slow call isn't a dead connection"
    );
    REGISTRY.lock().await.remove(&-1112);
}

/// SME-40 F1: listing tools for a turn waited the full connect timeout
/// (twice, with the retry) for every unreachable server, on every model
/// call. It now waits a bounded time and leaves the server out.
#[tokio::test]
async fn test_listing_tools_waits_a_bounded_time_for_a_hanging_server() {
    let hanging = config_at(-1103, start_hanging_server().await);
    let started = std::time::Instant::now();
    let tools = tokio::time::timeout(
        TOOL_LIST_WAIT + std::time::Duration::from_secs(2),
        tool_definitions_for(&test_pool(), &[hanging]),
    )
    .await
    .expect("listing tools should give up on a hanging server");
    assert!(tools.is_empty());
    assert!(started.elapsed() < TOOL_LIST_WAIT + std::time::Duration::from_secs(1));
}

/// SME-40 F1: a server that just failed isn't tried again on the next
/// turn; turns skip it until `RETRY_AFTER_FAILURE` has passed.
#[tokio::test]
async fn test_a_failed_server_isnt_retried_on_every_turn() {
    let (url, attempts) = start_refusing_server().await;
    let refusing = config_at(-1104, url);
    tool_definitions_for(&test_pool(), std::slice::from_ref(&refusing)).await;
    let after_first = attempts.load(Ordering::SeqCst);
    assert!(after_first > 0, "the first turn should try the server");

    tool_definitions_for(&test_pool(), std::slice::from_ref(&refusing)).await;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        after_first,
        "the next turn tried a server that had just failed"
    );
}

/// The status page is the user asking, so it tries even a server that
/// just failed.
#[tokio::test]
async fn test_the_status_check_retries_a_failed_server() {
    let (url, attempts) = start_refusing_server().await;
    let refusing = config_at(-1105, url);
    tool_definitions_for(&test_pool(), std::slice::from_ref(&refusing)).await;
    let after_first = attempts.load(Ordering::SeqCst);

    assert!(connection_check(&test_pool(), &refusing).await.is_err());
    assert!(
        attempts.load(Ordering::SeqCst) > after_first,
        "the status check should make a fresh attempt"
    );
}
