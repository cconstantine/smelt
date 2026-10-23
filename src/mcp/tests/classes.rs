//! An MCP tool's class from its server's annotations (SME-237).

use super::*;
use crate::anthropic::tools::ToolClass::{self, Destructive, ReadOnly, Writing};

fn class_of(annotations: serde_json::Value) -> ToolClass {
    let annotations: rmcp::model::ToolAnnotations =
        serde_json::from_value(annotations).expect("annotations from their wire names");
    class_from_annotations(Some(&annotations))
}

/// The design's mapping, from the wire names a server sends.
#[test]
fn test_annotations_map_to_classes_with_the_specs_defaults() {
    use serde_json::json;
    assert_eq!(class_from_annotations(None), Destructive, "no annotations");
    let cases = [
        (json!({"title": "Only a title"}), Destructive),
        (json!({"readOnlyHint": true}), ReadOnly),
        (json!({"readOnlyHint": true, "destructiveHint": true}), ReadOnly),
        (json!({"readOnlyHint": true, "destructiveHint": false}), ReadOnly),
        (json!({"readOnlyHint": false, "destructiveHint": false}), Writing),
        (json!({"destructiveHint": false}), Writing),
        (json!({"readOnlyHint": false, "destructiveHint": true}), Destructive),
        (json!({"readOnlyHint": false}), Destructive),
        (json!({"destructiveHint": true}), Destructive),
    ];
    for (annotations, expected) in cases {
        assert_eq!(class_of(annotations.clone()), expected, "{annotations}");
    }
}

/// Waits for the `tools/list_changed` that `enable_route` sends to mark
/// the connection's list stale.
async fn wait_until_stale(server_id: i64) {
    let stale = REGISTRY.lock().await.get(&server_id).expect("connected").stale.clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !stale.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("tools/list_changed marks the list stale");
}

/// Read from the connection's tool list at the call, after the re-list a
/// `tools/list_changed` asks for.
#[tokio::test]
async fn test_a_tools_class_comes_from_the_connections_current_list() {
    let server_id = -237_001;
    let server = register_test_connection(server_id, true).await;
    let config = test_config(server_id, "test-server");
    assert_eq!(tool_class(&test_pool(), &config, "echo").await, Destructive, "echo has no annotations");
    assert_eq!(tool_class(&test_pool(), &config, "no_such_tool").await, Destructive, "not listed");
    assert_eq!(tool_class(&test_pool(), &config, "second_tool").await, Destructive, "hidden, so not listed");

    server.router.write().await.enable_route("second_tool");
    wait_until_stale(server_id).await;
    let class = tool_class(&test_pool(), &config, "second_tool").await;
    REGISTRY.lock().await.remove(&server_id);
    assert_eq!(class, ReadOnly, "annotated readOnlyHint: true");
}

fn span_class(spans: &[opentelemetry_sdk::trace::SpanData], name: &str) -> Option<opentelemetry::Value> {
    let span = spans.iter().find(|span| span.name == format!("tool {name}")).expect("a tool span");
    span.attributes.iter().find(|kv| kv.key.as_str() == "smelt.tool.class").map(|kv| kv.value.clone())
}

/// An MCP call's class is on its `tool` span.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_mcp_calls_class_is_on_its_tool_span(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let server = register_test_connection(config.id, true).await;
    let echo = format!("mcp__{}__echo", config.name);
    let second = format!("mcp__{}__second_tool", config.name);
    let execute = |name: String| {
        let pool = pool.clone();
        async move {
            let (result, spans) = crate::telemetry::capture_spans(crate::anthropic::tools::execute(
                &pool,
                1,
                "toolu_237",
                &name,
                &serde_json::json!({}),
            ))
            .await;
            (result, span_class(&spans, &name))
        }
    };
    let (echoed, echo_class) = execute(echo).await;
    server.router.write().await.enable_route("second_tool");
    wait_until_stale(config.id).await;
    let (second_result, second_class) = execute(second).await;
    REGISTRY.lock().await.remove(&config.id);
    assert!(echoed.is_ok(), "{echoed:?}");
    assert!(second_result.is_ok(), "{second_result:?}");
    assert_eq!(echo_class, Some("destructive".into()));
    assert_eq!(second_class, Some("read-only".into()));
}
