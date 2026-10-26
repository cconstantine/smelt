//! The `tool {name}` span: its ids, its status, and none of the input.

use super::*;

/// SME-137: a tool call is a `tool {name}` span with the call's ids,
/// marked failed when the tool fails, and none of its input.
#[tokio::test]
async fn test_a_failed_tool_call_is_a_failed_span_without_its_input() {
    let (result, spans) = crate::telemetry::capture_spans(execute(
        &test_pool(),
        9_137_000_001,
        "toolu_9",
        "no_such_tool",
        &serde_json::json!({"text": "SECRET-INPUT"}),
    ))
    .await;
    assert!(result.is_err());
    let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
    assert_eq!(span.name, "tool no_such_tool");
    assert_eq!(span_attribute(span, "conversation_id"), Some(&9_137_000_001_i64.into()));
    assert_eq!(span_attribute(span, "gen_ai.tool.name"), Some(&"no_such_tool".into()));
    assert_eq!(span_attribute(span, "gen_ai.tool.call.id"), Some(&"toolu_9".into()));
    assert_eq!(span_attribute(span, "smelt.tool.truncated"), Some(&false.into()));
    let opentelemetry::trace::Status::Error { description } = &span.status else {
        panic!("{:?}", span.status)
    };
    assert!(description.contains("unknown tool"), "{description}");
    assert!(!format!("{span:?}").contains("SECRET-INPUT"), "{span:?}");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_mcp_tool_calls_span_names_its_server(pool: PgPool) {
    let (result, spans) = crate::telemetry::capture_spans(execute(
        &pool,
        9_137_000_002,
        "toolu_9",
        "mcp__github__search",
        &serde_json::json!({}),
    ))
    .await;
    assert!(result.is_err(), "no such server: {result:?}");
    let span = spans.iter().find(|span| span.name.starts_with("tool ")).expect("a tool span");
    assert_eq!(span_attribute(span, "smelt.tool.mcp_server"), Some(&"github".into()));
    // Review 1: an MCP tool's error can be its own output; the
    // `mcp call` span under it says what smelt saw.
    assert_eq!(
        span.status,
        opentelemetry::trace::Status::error(MCP_TOOL_FAILED),
        "no MCP output on the tool's span"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_tool_call_that_succeeds_leaves_its_span_unmarked(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let (result, spans) = crate::telemetry::capture_spans(execute(
        &pool,
        conversation.id,
        "toolu_9",
        "todoread",
        &serde_json::json!({}),
    ))
    .await;
    assert_eq!(result.as_deref(), Ok("[]"));
    let span = spans.iter().find(|span| span.name == "tool todoread").expect("a tool span");
    assert_eq!(span.status, opentelemetry::trace::Status::Unset);
}

