//! A script's MCP call, smelt's side (SME-359, T1–T13): `handle` against
//! a real database and a mock MCP server in `mcp::REGISTRY`. The path
//! through a live agent connection is in `agent_connection_tests`.

use super::*;
use crate::mcp::tests::{forget_mock_server, mock_server_for_tests};
use std::sync::atomic::Ordering;

/// A conversation with a live pod; with `chain`, an `active` agent whose
/// stored rule chain it is.
///
/// Its id comes from the fixture band: the limits' buckets are kept per
/// conversation id for the whole process, and every test's database
/// numbers its conversations from 1.
async fn conversation_with_pod(pool: &PgPool, chain: Option<Value>) -> (i64, i64) {
    sqlx::query("SELECT setval(pg_get_serial_sequence('conversations', 'id'), $1)")
        .bind(crate::id_alloc::FIXTURE_IDS.next_base())
        .execute(pool)
        .await
        .expect("move the conversation id sequence");
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
    let chain = json!([{"origin": "t13", "allow": ["*"], "deny": ["mock.app-t13/echo", "mock.app-t13/hang"]}]);
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

fn limits(per_minute: u32, burst: u32, max_in_flight: u32, per_command: i64) -> Limits {
    Limits { per_minute, burst, max_in_flight, per_command }
}

async fn call_under(pool: &PgPool, caller: Caller<'_>, tool: &str, limits: &Limits) -> Answer {
    handle_with(pool, caller, McpRequest::Call { body: json!({"tool": tool}).to_string() }, limits).await
}

/// T7, the rate: past the burst, a call is a 429 that says when to try
/// again, and is recorded as limited.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_calls_over_the_rate_are_held_back_with_a_retry_time(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t7").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let rate = limits(2, 2, 10, 1_000);
    let mut answers = Vec::new();
    for _ in 0..3 {
        answers.push(call_under(&pool, caller(conversation, pod), "mock-t7/second_tool", &rate).await);
    }
    forget_mock_server(config.id).await;
    assert_eq!(answers.iter().map(|a| a.status).collect::<Vec<_>>(), [200, 200, 429], "{answers:?}");
    let refused = parsed(&answers[2]);
    assert!(refused["retry_after_secs"].as_u64().is_some_and(|secs| secs > 0), "{refused}");
    assert!(refused["error"].as_str().expect("error").contains("SANDBOX_MCP_CALLS_PER_MINUTE"), "{refused}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // Review round 1, M2: a refusal by a limit writes no row (a runaway
    // loop would write one per refusal), but the summary still counts it.
    let outcomes: Vec<_> = db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows").into_iter().map(|r| r.outcome).collect();
    assert_eq!(outcomes, ["ok", "ok"]);
}

/// T7, in flight: a call while the cap's worth are in flight is refused
/// at once, not queued; once one ends, the next goes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_past_the_in_flight_cap_is_refused_at_once(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-t7b").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let one_at_a_time = limits(600, 10, 1, 1_000);
    let held = tokio::spawn({
        let pool = pool.clone();
        async move { call_under(&pool, caller(conversation, pod), "mock-t7b/hang", &one_at_a_time).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the first call reached the server");
    let started = std::time::Instant::now();
    let second = call_under(&pool, caller(conversation, pod), "mock-t7b/second_tool", &one_at_a_time).await;
    let waited = started.elapsed();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), held).await.expect("ends").expect("no panic");
    let third = call_under(&pool, caller(conversation, pod), "mock-t7b/second_tool", &one_at_a_time).await;
    forget_mock_server(config.id).await;
    assert_eq!((first.status, second.status, third.status), (502, 429, 200), "{second:?}");
    assert!(waited < std::time::Duration::from_millis(500), "refused at once, not queued: {waited:?}");
    assert!(second.body.contains("SANDBOX_MCP_MAX_IN_FLIGHT"), "{}", second.body);
}

/// T8: a command past its budget is held back, with no time to retry
/// after; another command has a budget of its own, and calls traced to no
/// command share one per pod.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_command_past_its_budget_is_held_back_and_another_command_isnt(pool: PgPool) {
    let (config, _calls) = mock_server_for_tests(&pool, "mock-t8").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal").id;
    for id in ["cmd-t8-a", "cmd-t8-b"] {
        db::create_terminal_command(&pool, conversation, terminal, id, "loop", 0).await.expect("command");
    }
    let budget = limits(600, 100, 10, 2);
    let mut seen = Vec::new();
    for command in [Some("cmd-t8-a"), Some("cmd-t8-a"), Some("cmd-t8-a"), Some("cmd-t8-b"), None, None, None] {
        let c = Caller { conversation_id: conversation, pod_id: pod, command_id: command };
        let answer = call_under(&pool, c, "mock-t8/second_tool", &budget).await;
        seen.push((command, answer.status, parsed(&answer).get("retry_after_secs").cloned()));
    }
    forget_mock_server(config.id).await;
    let statuses: Vec<_> = seen.iter().map(|(command, status, _)| (*command, *status)).collect();
    assert_eq!(
        statuses,
        [
            (Some("cmd-t8-a"), 200),
            (Some("cmd-t8-a"), 200),
            (Some("cmd-t8-a"), 429),
            (Some("cmd-t8-b"), 200),
            (None, 200),
            (None, 200),
            (None, 429),
        ]
    );
    assert_eq!(seen[2].2, None, "waiting doesn't help: a new command starts a new budget");
}

/// `SANDBOX_MCP_CALLS_PER_MINUTE=0` turns script calls off: refused
/// before anything is sent or recorded.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_zero_calls_a_minute_turns_script_calls_off(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-off").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let off = limits(0, 0, 4, 1_000);
    let call = call_under(&pool, caller(conversation, pod), "mock-off/second_tool", &off).await;
    let list = handle_with(&pool, caller(conversation, pod), McpRequest::List { server: None }, &off).await;
    forget_mock_server(config.id).await;
    assert_eq!((call.status, list.status), (403, 403), "{call:?}");
    assert!(call.body.contains("SANDBOX_MCP_CALLS_PER_MINUTE"), "{}", call.body);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows").is_empty());
}

/// C7: a command's summary is published as its calls finish, but a burst
/// of calls makes one event, not one each, and that event counts them all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_burst_of_calls_publishes_one_summary_with_them_all(pool: PgPool) {
    let (config, _calls) = mock_server_for_tests(&pool, "mock-c7").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal").id;
    db::create_terminal_command(&pool, conversation, terminal, "cmd-c7", "python3 loop.py", 0).await.expect("command");
    let mut events = crate::events::subscribe(conversation);
    let mine = Caller { conversation_id: conversation, pod_id: pod, command_id: Some("cmd-c7") };
    for _ in 0..3 {
        assert_eq!(call_body(&pool, mine, json!({"tool": "mock-c7/second_tool"})).await.status, 200);
    }
    let mut published = Vec::new();
    let deadline = tokio::time::Instant::now() + SUMMARY_EVERY * 3;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let crate::events::ConversationEvent::SandboxMcpCalls { command_id, summary } = event {
            published.push((command_id, summary.calls));
        }
    }
    forget_mock_server(config.id).await;
    assert_eq!(published, vec![("cmd-c7".to_string(), 3)]);
}

/// Review round 1, M3: a script may call only a tool its server lists,
/// by its listed name. A name that merely maps to one (another case, a
/// zero-width character, an embedded `/`) or one the server answers
/// without listing is a 404, and nothing reaches the server.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_tool_the_server_doesnt_list_is_a_404_and_unsent(pool: PgPool) {
    let (config, calls) = mock_server_for_tests(&pool, "mock-m3").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let c = caller(conversation, pod);
    let mut statuses = Vec::new();
    for tool in ["mock-m3/Echo", "mock-m3/ec\u{200b}ho", "mock-m3/echo/x", "mock-m3/fail", "mcp__mock-m3__Echo"] {
        statuses.push((tool, call_body(&pool, c, json!({"tool": tool})).await.status));
    }
    let listed = call_body(&pool, c, json!({"tool": "mock-m3/echo"})).await.status;
    forget_mock_server(config.id).await;
    assert!(statuses.iter().all(|(_, status)| *status == 404), "{statuses:?}");
    assert_eq!(listed, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "only the listed call was sent");
    assert_eq!(db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows").len(), 1);
}

/// Review round 1, M2: calls held back by a limit are counted in the
/// command's summary without a row each.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_calls_held_back_are_counted_without_rows(pool: PgPool) {
    let (config, _calls) = mock_server_for_tests(&pool, "mock-m2").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal").id;
    db::create_terminal_command(&pool, conversation, terminal, "cmd-m2", "loop", 0).await.expect("command");
    let mine = Caller { conversation_id: conversation, pod_id: pod, command_id: Some("cmd-m2") };
    let rate = limits(1, 1, 10, 1_000);
    let statuses: Vec<_> = futures_util::future::join_all((0..4).map(|_| call_under(&pool, mine, "mock-m2/second_tool", &rate)))
        .await
        .into_iter()
        .map(|a| a.status)
        .collect();
    forget_mock_server(config.id).await;
    assert_eq!(statuses.iter().filter(|s| **s == 429).count(), 3, "{statuses:?}");
    assert_eq!(db::list_sandbox_mcp_calls(&pool, conversation).await.expect("rows").len(), 1, "no row per refusal");
    let summary = command_summary(&pool, conversation, "cmd-m2").await.expect("read").expect("a summary");
    assert_eq!(summary.text(), "MCP: 1 call · mock-m2/second_tool ×1 · 3 held back by a limit");
}

/// SME-386 M4: a command's summary counts only its own conversation's
/// calls. A held-back call publishes the summary of the command id the
/// agent named, unchecked, so an agent naming another conversation's
/// command would otherwise read that command's counts.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_commands_summary_counts_only_its_own_conversations_calls(pool: PgPool) {
    let (mine, _) = conversation_with_pod(&pool, None).await;
    let (theirs, their_pod) = conversation_with_pod(&pool, None).await;
    let terminal = db::create_sandbox_terminal(&pool, their_pod).await.expect("terminal").id;
    db::create_terminal_command(&pool, theirs, terminal, "cmd-m4", "loop", 0).await.expect("command");
    db::insert_sandbox_mcp_call(&pool, theirs, their_pod, Some("cmd-m4"), "linear", "list_issues", Some("read_only"), "ok")
        .await
        .expect("their call");
    let theirs_read = command_summary(&pool, theirs, "cmd-m4").await.expect("read");
    assert!(theirs_read.is_some(), "the command's own conversation sees its call");
    let mine_read = command_summary(&pool, mine, "cmd-m4").await.expect("read");
    assert_eq!(mine_read, None, "another conversation's command is none of mine");
}

/// Review round 1, M2: a tool name over 256 bytes is refused before
/// anything is looked up or recorded.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_tool_name_over_256_bytes_is_a_400(pool: PgPool) {
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let long = format!("mock/{}", "x".repeat(252));
    let answer = call_body(&pool, caller(conversation, pod), json!({"tool": long})).await;
    assert_eq!(answer.status, 400, "{}", answer.body);
    assert!(answer.body.contains("256"), "{}", answer.body);
}

/// Review rounds 1 and 2 (M2): both budget counts, the command's and the
/// pod's command-less one, are answered from an index, never a scan of
/// every call: they run on every call, 429s included. A condition inside
/// a CASE can't use an index, so each is a plain query. With sequential
/// scans priced out, a plan that still scans the table is one that can't
/// use an index at all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_budget_counts_use_an_index(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("a connection");
    sqlx::query("SET enable_seqscan = off").execute(&mut *conn).await.expect("set");
    for name in ["command", "pod"] {
        let plan: Vec<String> = match name {
            "command" => sqlx::query_scalar(&format!("EXPLAIN {}", db::COMMAND_BUDGET_SQL)).bind("cmd-x").fetch_all(&mut *conn).await,
            _ => sqlx::query_scalar(&format!("EXPLAIN {}", db::POD_BUDGET_SQL)).bind(2_i64).fetch_all(&mut *conn).await,
        }
        .expect("explain");
        let plan = plan.join("\n");
        assert!(!plan.contains("Seq Scan on sandbox_mcp_calls"), "the {name} count scans the table:\n{plan}");
        assert!(plan.contains("Index") || plan.contains("Bitmap"), "the {name} count uses no index:\n{plan}");
    }
}

/// SME-386 M5: deleting a pod cascades to its calls, and the cascade's
/// own delete (Postgres runs `DELETE FROM ONLY … WHERE $1 = pod_id`) is
/// answered from an index. The partial index on command-less calls
/// can't serve it, so every pod delete scanned every call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pods_cascade_to_its_calls_uses_an_index(pool: PgPool) {
    let mut conn = pool.acquire().await.expect("a connection");
    sqlx::query("SET enable_seqscan = off").execute(&mut *conn).await.expect("set");
    let plan: Vec<String> = sqlx::query_scalar("EXPLAIN DELETE FROM ONLY sandbox_mcp_calls WHERE $1 = pod_id")
        .bind(2_i64)
        .fetch_all(&mut *conn)
        .await
        .expect("explain");
    let plan = plan.join("\n");
    assert!(!plan.contains("Seq Scan on sandbox_mcp_calls"), "the cascade scans the table:\n{plan}");
    assert!(plan.contains("Index") || plan.contains("Bitmap"), "the cascade uses no index:\n{plan}");
}

/// Review round 1, L4: listing the tools takes a rate token like a call,
/// so a loop of listings is held back too.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_listing_takes_a_rate_token(pool: PgPool) {
    let (config, _calls) = mock_server_for_tests(&pool, "mock-l4").await;
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let rate = limits(1, 1, 10, 1_000);
    let first = handle_with(&pool, caller(conversation, pod), McpRequest::List { server: None }, &rate).await;
    let second = handle_with(&pool, caller(conversation, pod), McpRequest::List { server: None }, &rate).await;
    let call = call_under(&pool, caller(conversation, pod), "mock-l4/second_tool", &rate).await;
    forget_mock_server(config.id).await;
    assert_eq!((first.status, second.status, call.status), (200, 429, 429), "{second:?}");
    assert!(second.body.contains("retry_after_secs"), "{}", second.body);
}

/// Review round 1, L6: the size cap is measured on the message the agent
/// gets, after the body's escaping, not on the body alone.
#[test]
fn test_an_answer_too_large_once_serialized_becomes_a_502() {
    // Under the body's 8 MiB cap, but every quote doubles when escaped.
    let quotes = Answer { status: 200, body: "\"".repeat(13 << 20) };
    let text = response_message(7, quotes);
    assert!(text.len() <= MAX_RESPONSE_MESSAGE_BYTES, "{} bytes", text.len());
    let message: crate::agent_protocol::ClientMessage = serde_json::from_str(&text).expect("a message");
    let crate::agent_protocol::ClientMessage::McpResponse { call_id: 7, status: 502, body } = message else { panic!("not a 502: {}", &text[..200]) };
    assert!(body.contains("bytes"), "{body}");
    let small = response_message(8, Answer { status: 200, body: "{}".into() });
    assert_eq!(small, r#"{"action":"mcp_response","call_id":8,"status":200,"body":"{}"}"#);
}

/// Review round 2, Low: a call that the caller's checks or the deployment
/// refuse never reaches the server, not even to connect or refresh a
/// grant: the checks come before the tool is looked up.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_caller_never_connects_to_the_server(pool: PgPool) {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    let counter = requests.clone();
    let app = axum::Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await });
    // An id of its own: the MCP registry is shared by every test.
    sqlx::query(&format!("ALTER TABLE mcp_servers ALTER COLUMN id RESTART WITH {}", crate::id_alloc::FIXTURE_IDS.next_base()))
        .execute(&pool)
        .await
        .expect("move the server id");
    let config = db::create_mcp_server_config(&pool, "counted-r2", &url, &std::collections::HashMap::new(), "static_headers", None, None)
        .await
        .expect("a server row");
    let (conversation, pod) = conversation_with_pod(&pool, None).await;
    let off = call_under(&pool, caller(conversation, pod), "counted-r2/echo", &limits(0, 0, 4, 1_000)).await;
    sqlx::query("UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1").bind(pod).execute(&pool).await.expect("stop");
    let stopped = call_under(&pool, caller(conversation, pod), "counted-r2/echo", &limits(60, 20, 4, 1_000)).await;
    crate::mcp::evict(config.id).await;
    assert_eq!((off.status, stopped.status), (403, 503), "{off:?} / {stopped:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 0, "the server was contacted");
}
