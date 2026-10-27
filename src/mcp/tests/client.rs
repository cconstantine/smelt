use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ensure_default_servers_adds_exa_search(pool: sqlx::PgPool) {
    ensure_default_servers(&pool).await;
    let exa = crate::db::get_mcp_server_config_by_name(&pool, "exa")
        .await
        .expect("look up exa")
        .expect("exa should be added at startup");
    assert_eq!(exa.url, EXA_MCP_URL);
    assert!(exa.extra_headers.0.is_empty(), "keyless by default");
}

/// The real, keyless Exa service through smelt's own MCP client (the
/// full `rmcp` handshake, not a bare `tools/call`). Needs the internet
/// and depends on Exa's unpublished free limits, so it only runs when
/// `SMELT_LIVE_EXA=1` is set — never in CI, which runs every ignored
/// test in its browser job:
/// `SMELT_LIVE_EXA=1 scripts/cluster-test --features server -- --ignored --exact mcp::tests::client::test_live_exa_search_through_smelt_mcp_client`
/// (a connect reads its server's row, so it needs the test database).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
#[ignore]
async fn test_live_exa_search_through_smelt_mcp_client(pool: sqlx::PgPool) {
    if std::env::var("SMELT_LIVE_EXA").as_deref() != Ok("1") {
        eprintln!("skipped: set SMELT_LIVE_EXA=1 to run against the real Exa service");
        return;
    }
    let config = create_static_server(&pool, EXA_MCP_URL).await;

    let tools = connection_check(&pool, &config)
        .await
        .expect("smelt's MCP client should connect to Exa keylessly");
    assert_eq!(tools, vec!["web_search_exa".to_string()], "only search should be offered");

    let result = call_tool(
        &pool,
        &config,
        "web_search_exa",
        serde_json::json!({"query": "tokio broadcast channel receiver_count", "numResults": 2}),
    )
    .await
    .expect("a keyless search should succeed");
    assert!(result.contains("https://"), "expected result URLs, got: {result}");
    evict(config.id).await;
}

#[test]
fn test_parse_tool_name_splits_server_and_tool() {
    assert_eq!(
        parse_tool_name("mcp__github__list_issues"),
        Some(("github", "list_issues"))
    );
    // A tool name that itself contains further underscores still
    // resolves — only the first `__` after the server name matters.
    assert_eq!(
        parse_tool_name("mcp__github__list__issues"),
        Some(("github", "list__issues"))
    );
    assert_eq!(
        parse_tool_name("read_file"),
        None,
        "native tool names must not be misparsed as MCP-dispatched"
    );
    assert_eq!(parse_tool_name("mcp__no_separator"), None);
}

#[test]
fn test_build_header_map_rejects_invalid_header_values() {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer ok".to_string());
    assert!(build_header_map(&headers).is_ok());

    let mut invalid = HashMap::new();
    // A raw newline is not a legal header value.
    invalid.insert("Authorization".to_string(), "Bearer bad\nvalue".to_string());
    assert!(build_header_map(&invalid).is_err());
}

#[tokio::test]
async fn test_tool_definitions_for_merges_and_namespaces_tools_from_connected_servers() {
    let server_id = -1001;
    register_test_connection(server_id, false).await;

    let definitions =
        tool_definitions_for(&test_pool(), &[test_config(server_id, "test-server")], &unruled()).await;

    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "mcp__test-server__echo");
    assert_eq!(definitions[0].description, "Echoes its input back");

    REGISTRY.lock().await.remove(&server_id);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_call_tool_dispatches_and_returns_text_content(pool: sqlx::PgPool) {
    let server_id = -1002;
    register_test_connection(server_id, false).await;

    let result = call_tool(
        &pool,
        &test_config(server_id, "test-server"),
        "echo",
        serde_json::json!({"hello": "world"}),
    )
    .await
    .expect("call_tool should succeed");

    assert!(
        result.contains("hello"),
        "expected echoed arguments in result, got: {result}"
    );
    assert!(result.contains("world"));

    REGISTRY.lock().await.remove(&server_id);
}
