//! `execute`'s dispatch: unknown names, `mcp__` routing, and the registry.

use super::*;

#[tokio::test]
async fn test_unknown_tool_name_errors() {
    let pool = test_pool();
    let result = execute(&pool, 1, "toolu_1", "bogus", &serde_json::json!({})).await;
    assert!(result.is_err(), "expected an error, got {result:?}");
}

/// `execute`'s new `mcp__` routing branch, exercised end to end
/// against a real (empty) `mcp_servers` table — needs a real
/// `#[sqlx::test]` pool (unlike `test_pool()`'s lazy fake one)
/// since this path genuinely queries the database, unlike
/// `add`/`count`. Covers the "config was deleted out from under a
/// stale tool name" case `resolve`'s doc comment describes —
/// a plain tool error, not a panic.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_execute_routes_mcp_prefixed_names_and_errors_clearly_on_unknown_server(
    pool: PgPool,
) {
    let result = execute(
        &pool,
        1,
        "toolu_mcp",
        "mcp__github__list_issues",
        &serde_json::json!({}),
    )
    .await;

    let err = result.expect_err("no such MCP server is configured, so this must fail");
    assert!(
        err.contains("github"),
        "error should name the unresolvable server, got: {err}"
    );
}

/// A native tool name is unaffected by the new `mcp__` routing
/// branch — `parse_tool_name` only matches the `mcp__` prefix, so
/// ordinary dispatch (and its lazy/never-connects `test_pool`) is
/// unchanged.
/// Dispatch finds a tool by name, so two tools of one name would
/// make the second unreachable while still offering it.
#[test]
fn test_every_native_tool_name_is_unique() {
    let mut names: Vec<String> = native_tool_definitions().into_iter().map(|t| t.name).collect();
    let count = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), count, "a tool name is registered twice");
}

/// Anthropic refuses a request whose any tool name isn't
/// `^[a-zA-Z0-9_-]{1,128}$` (SME-365); smelt's own tools are in every one.
#[test]
fn test_every_native_tool_name_matches_anthropics_pattern() {
    for tool in native_tool_definitions() {
        let ok = (1..=128).contains(&tool.name.len()) && tool.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        assert!(ok, "{:?}", tool.name);
    }
}

#[tokio::test]
async fn test_execute_still_dispatches_native_tools_normally() {
    let pool = test_pool();
    // The name resolves to smelt's own `http_request`, not "unknown
    // tool": the call gets as far as its rule chain, whose read on a pool
    // that never connects refuses it (SME-354 B10: the call path fails
    // closed), naming the tool it resolved.
    let result = execute(&pool, 1, "toolu_http", "http_request", &serde_json::json!({})).await;
    let error = result.expect_err("refused at the chain read");
    assert!(
        error.starts_with("couldn't read this conversation's tool rules, so http_request is refused:"),
        "{error}"
    );
}

