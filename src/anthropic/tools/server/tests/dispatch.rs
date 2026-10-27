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

#[tokio::test]
async fn test_execute_still_dispatches_native_tools_normally() {
    let pool = test_pool();
    // `http_request` refuses a missing url before any I/O: the
    // error is the tool's own, not "unknown tool".
    let result = execute(&pool, 1, "toolu_http", "http_request", &serde_json::json!({})).await;
    assert_eq!(result, Err("missing or non-string field: url".to_string()));
}

