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

/// The class `call_tool_classed` hands over for a call to `tool`, on
/// `pool`'s database. Since SME-240 the call reads that pool for the
/// tool's class override, so every test of a call needs a real
/// (`#[sqlx::test]`) pool — `test_pool()`'s fake errors on the read and
/// the call fails closed before sending.
async fn class_of_call_on(pool: &sqlx::PgPool, config: &McpServerConfig, tool: &str) -> Option<ToolClass> {
    let class = std::sync::Mutex::new(None);
    let _ = call_tool_classed(pool, config, tool, serde_json::json!({}), |c| {
        *class.lock().unwrap() = Some(c);
        Ok(())
    })
    .await;
    class.into_inner().unwrap()
}

/// Read from the connection's tool list at the call, after the re-list a
/// `tools/list_changed` asks for.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_tools_class_comes_from_the_connections_current_list(pool: sqlx::PgPool) {
    let server_id = -237_001;
    let server = register_test_connection(server_id, true).await;
    let config = test_config(server_id, "test-server");
    assert_eq!(class_of_call_on(&pool, &config, "echo").await, Some(Destructive), "echo has no annotations");
    assert_eq!(class_of_call_on(&pool, &config, "no_such_tool").await, Some(Destructive), "not listed");
    assert_eq!(class_of_call_on(&pool, &config, "second_tool").await, Some(Destructive), "hidden, so not listed");

    server.router.write().await.enable_route("second_tool");
    wait_until_stale(server_id).await;
    let class = class_of_call_on(&pool, &config, "second_tool").await;
    REGISTRY.lock().await.remove(&server_id);
    assert_eq!(class, Some(ReadOnly), "annotated readOnlyHint: true");
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

/// Review 1: an MCP call to a server that can't be reached says why,
/// as before the class: the class lookup isn't a connect of its own,
/// whose failure would leave the call only "failed to connect recently".
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unreachable_servers_call_says_why_it_failed(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, "http://127.0.0.1:1/mcp").await;
    let name = format!("mcp__{}__echo", config.name);
    let result = crate::anthropic::tools::execute(&pool, 1, "toolu_237", &name, &serde_json::json!({})).await;
    lock_failures().remove(&config.id);
    let error = result.expect_err("nothing listens on port 1");
    assert!(error.contains("failed to connect to MCP server"), "{error}");
    assert!(!error.contains("failed to connect recently"), "{error}");
}

/// Review 1: arguments that aren't an object are refused before any
/// connect, so they neither wait for one nor start the skip window.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_mcp_call_with_bad_arguments_is_refused_before_connecting(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, "http://127.0.0.1:1/mcp").await;
    let name = format!("mcp__{}__echo", config.name);
    let result = crate::anthropic::tools::execute(&pool, 1, "toolu_237", &name, &serde_json::json!(5)).await;
    let failed = failed_recently(config.id);
    lock_failures().remove(&config.id);
    let error = result.expect_err("5 isn't an object");
    assert!(error.contains("tool arguments must be a JSON object"), "{error}");
    assert!(!failed, "no connect was tried, so none failed");
}

/// SME-240 test 8, made cheap by the fix above: a call whose pool can't
/// answer the class-override read (the fake `test_pool()` never
/// connects) fails before sending, with the read's own message — no
/// silent fallback to the annotations, and the server is never asked:
/// the mock would have answered `echo`, and the call comes back refused.
#[tokio::test]
async fn test_a_call_refuses_when_its_class_override_row_cannot_be_read() {
    let server_id = -240_003;
    register_test_connection(server_id, false).await;
    let config = test_config(server_id, "test-server");
    let failed = call_tool(&test_pool(), &config, "echo", serde_json::json!({})).await;
    REGISTRY.lock().await.remove(&server_id);
    let error = failed.expect_err("an unreadable override row fails the call, not falls back");
    assert!(error.contains("class override"), "{error}");
}

/// SME-240: the user's override wins at the call, and clearing it hands
/// the answer back to the server — all on one connection, no `evict`, so
/// this is also the proof there is no per-connection class cache: the
/// override is re-read at every call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_override_wins_at_the_call_and_a_clear_gives_the_server_its_answer_back(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let server = register_test_connection(config.id, true).await;
    server.router.write().await.enable_route("second_tool");
    wait_until_stale(config.id).await;

    assert_eq!(class_of_call_on(&pool, &config, "second_tool").await, Some(ReadOnly), "the server's answer first");
    crate::db::set_mcp_tool_class(&pool, config.id, "second_tool", Destructive)
        .await
        .expect("set a destructive override");
    assert_eq!(class_of_call_on(&pool, &config, "second_tool").await, Some(Destructive), "the override wins");
    crate::db::clear_mcp_tool_class(&pool, config.id, "second_tool")
        .await
        .expect("clear the override");
    let class = class_of_call_on(&pool, &config, "second_tool").await;
    REGISTRY.lock().await.remove(&config.id);
    assert_eq!(class, Some(ReadOnly), "the same connection gives the server's answer back");
}

/// SME-240: an override exists whether or not the server lists the tool
/// — it classes the call where the annotation rule alone says
/// destructive (unlisted). The mock refuses the call itself (the name
/// isn't in its router); the class was handed over before sending.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_override_for_a_tool_the_server_does_not_list_classes_the_call(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let _server = register_test_connection(config.id, false).await;
    crate::db::set_mcp_tool_class(&pool, config.id, "no_such_tool", ReadOnly)
        .await
        .expect("set a read-only override for an unlisted tool");
    let class = class_of_call_on(&pool, &config, "no_such_tool").await;
    REGISTRY.lock().await.remove(&config.id);
    assert_eq!(class, Some(ReadOnly), "the override classes the call; the server never answered it");
}

/// SME-241: the stored chain rules the *offer* too. A `class: read-only`
/// entry hides the mock's unannotated (so destructive) `echo` and keeps
/// the annotated read-only `second_tool`; a saved read-only override —
/// the same read the call path makes — shows `echo` again; and a deny
/// pattern naming the server hides a tool its class alone would keep.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_chain_rules_which_mcp_tools_are_offered(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let server = register_test_connection(config.id, true).await;
    server.router.write().await.enable_route("second_tool");
    wait_until_stale(config.id).await;
    let echo = format!("mcp__{}__echo", config.name);
    let second = format!("mcp__{}__second_tool", config.name);

    let read_only = crate::personas::rules::RuleChain::from_stored(Some(serde_json::json!([{"class": "read-only"}])));
    let names = tool_definitions_for(&pool, std::slice::from_ref(&config), &read_only)
        .await
        .into_iter()
        .map(|d| d.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&second), "annotated read-only stays: {names:?}");
    assert!(!names.contains(&echo), "unannotated means destructive, so the ceiling hides it: {names:?}");

    crate::db::set_mcp_tool_class(&pool, config.id, "echo", ReadOnly).await.expect("read-only override");
    let names = tool_definitions_for(&pool, std::slice::from_ref(&config), &read_only)
        .await
        .into_iter()
        .map(|d| d.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&echo), "the user's override rules the offer as it rules the call: {names:?}");
    crate::db::clear_mcp_tool_class(&pool, config.id, "echo").await.expect("clear the override");

    let denied = crate::personas::rules::RuleChain::from_stored(Some(serde_json::json!([
        { "class": "all", "deny": [format!("{}/second_tool", config.name)] }
    ])));
    let names = tool_definitions_for(&pool, std::slice::from_ref(&config), &denied)
        .await
        .into_iter()
        .map(|d| d.name)
        .collect::<Vec<_>>();
    assert!(!names.contains(&second), "the server's deny pattern hides just it: {names:?}");
    assert!(names.contains(&echo), "and only it: {names:?}");
    REGISTRY.lock().await.remove(&config.id);
}

/// SME-241: a chain that denies the class refuses an MCP call before its
/// server is asked. The mock would have answered `echo`, so the refusal
/// and the mock's own counter together say the send never happened; the
/// message names the rule and the tool as the model calls it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_denied_mcp_call_is_refused_before_its_server_is_asked(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let server = register_test_connection(config.id, false).await;
    let conversation = agent_conversation(&pool, serde_json::json!([{ "class": "read-only" }])).await;
    let name = format!("mcp__{}__echo", config.name);
    let result =
        crate::anthropic::tools::execute(&pool, conversation, "toolu_241", &name, &serde_json::json!({"q": "SECRET-ARG"})).await;
    REGISTRY.lock().await.remove(&config.id);
    let error = result.expect_err("a read-only ceiling denies the destructive echo");
    assert_eq!(
        error,
        format!("{name} is denied by this conversation's tool rules (class ceiling read-only doesn't allow a destructive tool)")
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 0, "the server was never asked");
}

/// SME-240's ordering, kept: the override read still speaks first. A rule
/// can't be evaluated without the class that read answers with, so a call
/// whose override row can't be read fails with the read's message and not
/// the rule's. Green by construction — this is the guard, not the new
/// behaviour.
#[tokio::test]
async fn test_a_rule_check_still_waits_for_the_class_override_read() {
    let server_id = -241_001;
    register_test_connection(server_id, false).await;
    let config = test_config(server_id, "test-server");
    let failed =
        call_tool_classed(&test_pool(), &config, "echo", serde_json::json!({}), |_| Err("the chain denies it".to_string())).await;
    REGISTRY.lock().await.remove(&server_id);
    let error = failed.expect_err("the override read fails the call first");
    assert!(error.contains("class override"), "{error}");
    assert!(!error.contains("the chain denies it"), "{error}");
}
