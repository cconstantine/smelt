//! A script's MCP call, smelt's side (SME-359, T1–T13): `handle` against
//! a real database and a mock MCP server in `mcp::REGISTRY`. The path
//! through a live agent connection is in `agent_connection_tests`.

use super::*;
use crate::mcp::tests::{forget_mock_server, mock_server_for_tests};
use std::sync::atomic::Ordering;

/// A conversation with a live pod; with `chain`, an `active` agent whose
/// stored rule chain it is.
async fn conversation_with_pod(pool: &PgPool, chain: Option<Value>) -> (i64, i64) {
    let conversation = db::create_conversation(pool).await.expect("conversation").id;
    if let Some(chain) = chain {
        sqlx::query(
            "INSERT INTO agents (conversation_id, root_id, depth, brief, state, rule_chain) VALUES ($1, $1, 1, 'b', 'active', $2)",
        )
        .bind(conversation)
        .bind(chain)
        .execute(pool)
        .await
        .expect("an agent row");
    }
    let pod = db::create_sandbox_pod(pool, conversation).await.expect("pod row").id;
    (conversation, pod)
}

fn caller(conversation_id: i64, pod_id: i64) -> Caller<'static> {
    Caller { conversation_id, pod_id, command_id: None }
}

async fn call_body(pool: &PgPool, caller: Caller<'_>, body: Value) -> Answer {
    handle(pool, caller, McpRequest::Call { body: body.to_string() }).await
}

fn parsed(answer: &Answer) -> Value {
    serde_json::from_str(&answer.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", answer.body))
}

/// T1, and T4's replacement (Q1: no script ceiling): an allowed call
/// returns the server's content whole, past the model's 30,000-character
/// cut, and an unannotated (destructive) tool goes with the chain
/// allow-all, as the model's call does.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_allowed_call_returns_the_whole_result(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t1").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let long = "x".repeat(40_000);
    let answer = call_body(&pool, caller(conversation, pod), json!({"tool": "mock-t1/echo", "arguments": {"long": long}})).await;
    forget_mock_server(config.id).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let body = parsed(&answer);
    let text = body["text"].as_str().expect("text");
    assert!(text.contains(&long), "the 40,000 characters, uncut: {} long", text.len());
    assert_eq!(body["content"][0]["type"], "text");
    assert!(body["structured_content"].is_null());
    assert!(body.get("is_error").is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let rows = db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].outcome.as_str(), rows[0].class.as_deref()), ("ok", Some("destructive")));
}

/// T2: the chain denies the tool by name; the refusal names the rule, and
/// nothing reaches the server.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_tool_the_chain_denies_is_refused_unsent(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t2").await;
    let chain = json!([{"origin": "persona t2", "allow": ["*"], "deny": ["mock-t2/second_tool"]}]);
    let (conversation, pod) = conversation_with_pod(&pool, Some(chain)).await;
    let answer = call_body(&pool, caller(conversation, pod), json!({"tool": "mock-t2/second_tool"})).await;
    forget_mock_server(config.id).await;
    assert_eq!(answer.status, 403, "{}", answer.body);
    let error = parsed(&answer)["error"].as_str().expect("error").to_string();
    assert!(error.contains("denied") && error.contains("mock-t2/second_tool"), "the rule's own text: {error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a refused call is never sent");
    let rows = db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows");
    assert_eq!((rows[0].outcome.as_str(), rows[0].class.as_deref()), ("refused", Some("read-only")));
}

/// T3: a chain that can't be read refuses the call, as the model's
/// (SME-354 B10).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unreadable_chain_refuses_the_call(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t3").await;
    let (conversation, pod) = conversation_with_pod(&pool, Some(json!("not a chain"))).await;
    let answer = call_body(&pool, caller(conversation, pod), json!({"tool": "mock-t3/echo"})).await;
    forget_mock_server(config.id).await;
    assert_eq!(answer.status, 403, "{}", answer.body);
    assert!(answer.body.contains("couldn't read this conversation's tool rules"), "{}", answer.body);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let rows = db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows");
    assert_eq!((rows[0].outcome.as_str(), rows[0].class.as_deref()), ("refused", None));
}

/// T6: only an active agent's scripts call; a finished or stopped one's
/// are refused before anything is recorded.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_agent_that_isnt_active_cant_call(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t6").await;
    let (conversation, pod) = conversation_with_pod(&pool, Some(json!([]))).await;
    let mut seen = Vec::new();
    for state in ["finished", "stopped", "active"] {
        sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
            .bind(conversation)
            .bind(state)
            .execute(&pool)
            .await
            .expect("set state");
        let answer = call_body(&pool, caller(conversation, pod), json!({"tool": "mock-t6/second_tool"})).await;
        seen.push((state, answer.status));
    }
    forget_mock_server(config.id).await;
    assert_eq!(seen, vec![("finished", 403), ("stopped", 403), ("active", 200)]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows").len(), 1);
}

/// A pod that is no longer the conversation's live one, or another
/// conversation's, can't call for it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_or_foreign_pod_cant_call(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-pods").await;
    let (a, pod_a) = conversation_with_pod(&pool, None).await;
    let (_b, pod_b) = conversation_with_pod(&pool, None).await;
    let foreign = call_body(&pool, caller(a, pod_b), json!({"tool": "mock-pods/echo"})).await;
    sqlx::query("UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1").bind(pod_a).execute(&pool).await.expect("stop");
    let stopped = call_body(&pool, caller(a, pod_a), json!({"tool": "mock-pods/echo"})).await;
    forget_mock_server(config.id).await;
    assert_eq!((foreign.status, stopped.status), (503, 503), "{} / {}", foreign.body, stopped.body);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// Names a script may use, and requests that aren't calls.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_names_its_tool_either_way_and_a_bad_request_is_refused(pool: PgPool) {
    let (config, _calls) = mock_server_for_tests(&pool, "mock.app-names").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let c = caller(conversation, pod);
    let by_rule_name = call_body(&pool, c, json!({"tool": "mock.app-names/second_tool"})).await;
    let model_name = crate::mcp::namespaced_tool_name("mock.app-names", "second_tool");
    let by_model_name = call_body(&pool, c, json!({"tool": model_name})).await;
    let unknown_server = call_body(&pool, c, json!({"tool": "nope/echo"})).await;
    let no_tool = call_body(&pool, c, json!({"arguments": {}})).await;
    let bad_arguments = call_body(&pool, c, json!({"tool": "mock.app-names/echo", "arguments": [1]})).await;
    let not_json = handle(&pool, c, McpRequest::Call { body: "{oops".into() }).await;
    forget_mock_server(config.id).await;
    assert_eq!(
        [by_rule_name.status, by_model_name.status, unknown_server.status, no_tool.status, bad_arguments.status, not_json.status],
        [200, 200, 404, 400, 400, 400]
    );
    let rows = db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows");
    assert_eq!(rows.len(), 2, "a request refused before the call has no row");
    assert!(rows.iter().all(|row| row.server == "mock.app-names" && row.tool == "second_tool" && row.conversation_id == conversation), "{rows:?}");
}

/// T10: the record holds the outcome, class, command and times, and
/// neither it nor any span holds the arguments or the result. A command
/// id that isn't this conversation's is not kept.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_record_and_spans_hold_no_arguments_or_results(pool: PgPool) {
    let (config, _calls) = mock_server_for_tests(&pool, "mock-t10").await;
    let chain = json!([{"origin": "t10", "allow": ["*"], "deny": ["mock-t10/second_tool"]}]);
    let (conversation, pod) = conversation_with_pod(&pool, Some(chain)).await;
    let (other, other_pod) = conversation_with_pod(&pool, None).await;
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal").id;
    db::create_terminal_command(&pool, conversation, terminal, "cmd-t10", "python3 x.py", 0).await.expect("command");
    let other_terminal = db::create_sandbox_terminal(&pool, other_pod).await.expect("terminal").id;
    db::create_terminal_command(&pool, other, other_terminal, "cmd-other", "x", 0).await.expect("command");
    let marker = "SCRIPT-ARG-MARKER-359";
    let ((ok, refused, foreign), spans) = crate::telemetry::capture_spans(async {
        let mine = Caller { conversation_id: conversation, pod_id: pod, command_id: Some("cmd-t10") };
        let ok = call_body(&pool, mine, json!({"tool": "mock-t10/echo", "arguments": {"secret": marker}})).await;
        let refused = call_body(&pool, mine, json!({"tool": "mock-t10/second_tool", "arguments": {"secret": marker}})).await;
        let theirs = Caller { command_id: Some("cmd-other"), ..mine };
        let foreign = call_body(&pool, theirs, json!({"tool": "mock-t10/echo"})).await;
        (ok, refused, foreign)
    })
    .await;
    forget_mock_server(config.id).await;
    assert_eq!((ok.status, refused.status, foreign.status), (200, 403, 200));
    assert!(ok.body.contains(marker), "the result echoes the arguments");
    let rows = db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows");
    let summary: Vec<_> = rows.iter().map(|r| (r.outcome.as_str(), r.class.as_deref(), r.command_id.as_deref(), r.pod_id)).collect();
    assert_eq!(
        summary,
        vec![
            ("ok", Some("destructive"), Some("cmd-t10"), pod),
            ("refused", Some("read-only"), Some("cmd-t10"), pod),
            ("ok", Some("destructive"), None, pod),
        ]
    );
    assert!(rows.iter().all(|r| r.finished_at.is_some_and(|f| f >= r.started_at)), "{rows:?}");
    assert!(!format!("{rows:?}").contains(marker));
    let script_spans: Vec<_> = spans.iter().filter(|span| span.name == "script mcp call").collect();
    assert_eq!(script_spans.len(), 3, "{spans:?}");
    assert!(
        script_spans[0].attributes.iter().any(|kv| kv.key.as_str() == "command_id" && kv.value == "cmd-t10".into()),
        "{:?}",
        script_spans[0].attributes
    );
    assert!(!format!("{spans:?}").contains(marker), "no argument or result in a span");
}

/// Holds a call to the mock's `hang` tool in flight, runs `meanwhile`,
/// and returns the call's answer once its 1 s test budget runs out.
async fn with_a_call_in_flight<F: std::future::Future<Output = ()>>(
    pool: &PgPool,
    caller: Caller<'static>,
    calls: &std::sync::atomic::AtomicU32,
    tool: &str,
    meanwhile: F,
) -> Answer {
    let pool_for_call = pool.clone();
    let body = json!({"tool": tool}).to_string();
    let call = tokio::spawn(async move { handle(&pool_for_call, caller, McpRequest::Call { body }).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the call reached the server");
    meanwhile.await;
    tokio::time::timeout(std::time::Duration::from_secs(10), call).await.expect("the call ends").expect("no panic")
}

/// T11: the conversation deleted while a call is in flight: the call ends
/// on its own budget, and there is nothing left to record.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversation_deleted_mid_call_leaves_nothing(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t11").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let answer = with_a_call_in_flight(&pool, caller(conversation, pod), &calls, "mock-t11/hang", async {
        sqlx::query("DELETE FROM conversations WHERE id = $1").bind(conversation).execute(&pool).await.expect("delete");
    })
    .await;
    forget_mock_server(config.id).await;
    assert_eq!(answer.status, 502, "{}", answer.body);
    assert!(db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows").is_empty());
}

/// T12: the pod stopped while a call is in flight: the call ends and its
/// row says how.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pod_stopped_mid_call_still_finishes_its_row(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t12").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let answer = with_a_call_in_flight(&pool, caller(conversation, pod), &calls, "mock-t12/hang", async {
        sqlx::query("UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1").bind(pod).execute(&pool).await.expect("stop");
    })
    .await;
    forget_mock_server(config.id).await;
    assert_eq!(answer.status, 502, "{}", answer.body);
    let rows = db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "failed");
    assert!(rows[0].finished_at.is_some());
}

/// T13: the listing is what the chain allows, with the servers' own names,
/// classes and schemas.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_listing_is_what_the_chain_allows(pool: PgPool) {
    let (config, _calls) = mock_server_for_tests(&pool, "mock.app-t13").await;
    let chain = json!([{"origin": "t13", "allow": ["*"], "deny": ["mock.app-t13/echo"]}]);
    let (conversation, pod) = conversation_with_pod(&pool, Some(chain)).await;
    let c = caller(conversation, pod);
    let all = handle(&pool, c, McpRequest::List { server: None }).await;
    let one = handle(&pool, c, McpRequest::List { server: Some("mock.app-t13".into()) }).await;
    let none = handle(&pool, c, McpRequest::List { server: Some("no-such-server".into()) }).await;
    forget_mock_server(config.id).await;
    assert_eq!((all.status, one.status, none.status), (200, 200, 404));
    let tools = parsed(&one)["tools"].as_array().expect("tools").clone();
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0]["name"], "mock.app-t13/second_tool");
    assert_eq!(
        (tools[0]["server"].as_str(), tools[0]["tool"].as_str(), tools[0]["class"].as_str()),
        (Some("mock.app-t13"), Some("second_tool"), Some("read-only"))
    );
    assert_eq!(tools[0]["input_schema"]["type"], "object");
    let all_names: Vec<_> = parsed(&all)["tools"].as_array().expect("tools").iter().map(|t| t["name"].clone()).collect();
    assert!(all_names.contains(&json!("mock.app-t13/second_tool")) && !all_names.contains(&json!("mock.app-t13/echo")), "{all_names:?}");
}
