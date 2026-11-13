//! The subscription's usage windows through a turn's real caller paths
//! (SME-486): the fake CLI streams `rate_limit_event`s, `run::read` and
//! `once` hold them, and `turn::call` records them for the turn's
//! provider, whether the call ended Ok or Err.

use super::*;
use crate::claude_cli::tests::usage::{MARKER, planted_event};

/// `provider_id`'s rows: (window, utilization, status).
async fn windows(pool: &PgPool, provider_id: i64) -> Vec<(String, f64, String)> {
    db::provider_usage_windows(pool, provider_id)
        .await
        .expect("the windows")
        .into_iter()
        .map(|r| (r.window_name, r.utilization, r.status))
        .collect()
}

/// `capped.jsonl`'s one `rate_limit_event`, as captured.
fn captured_event() -> Value {
    fixture_lines("capped").into_iter().find(|l| l["type"] == "rate_limit_event").expect("the capture's event")
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_records_its_providers_windows(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("usage-turn", vec![vec![json!({"read": true}), json!({"emit": [captured_event()]}), text_reply("hi")]]);
    let provider = fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn runs");

    assert_eq!(
        windows(&pool, provider).await,
        vec![("five_hour".to_string(), 0.09, "allowed".to_string()), ("seven_day".to_string(), 0.32, "allowed".to_string())]
    );
}

/// Round 1 A-L1: a reading left held by an earlier call (a Stop, a run
/// settled in the background) is dropped when the next Claude Code call
/// starts, not stored as that call's provider's.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stale_held_reading_is_not_stored_by_the_next_call(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("usage-stale", vec![vec![json!({"read": true}), text_reply("hi")]]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    crate::claude_cli::usage::seen(conversation.id, &captured_event());

    run_turn(&pool, conversation.id, hello()).await.expect("the turn runs");

    assert!(windows(&pool, provider).await.is_empty(), "the call streamed no event of its own");
    assert_eq!(crate::claude_cli::usage::take(conversation.id), None, "and nothing is left held");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_capped_turn_still_records(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // Synthesized, as errors.rs's limit test: no capture of a real limit.
    let limit = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1_767_225_600,
        "unifiedWindows": {"five_hour": {"utilization": 1.0, "status": "rejected", "resetsAt": 1_767_225_600}}}});
    let refused = json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": 429, "result": "API Error: 429 rate limited"});
    let fake = FakeCli::new("usage-capped", vec![vec![json!({"read": true}), json!({"emit": [limit, refused]}), json!({"exit": 1})]]);
    let provider = fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("limited");

    assert!(error.to_string().contains("Your Claude subscription's usage limit (five-hour) is reached"), "the message is unchanged: {error}");
    assert_eq!(windows(&pool, provider).await, vec![("five_hour".to_string(), 1.0, "rejected".to_string())]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_summary_records(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let full = {
        let mut lines = api_call(31_000, vec![text_block("one")], "end_turn", 3);
        lines.push(crate::claude_cli::tests::fake::result_ok());
        json!({"emit": lines})
    };
    // Only the summary's one-shot process streams an event.
    let fake = FakeCli::new("usage-summary", vec![
        vec![json!({"read": true}), full],
        vec![json!({"read": true}), json!({"emit": [captured_event()]}), text_reply("SUMMARY-USAGE")],
        vec![json!({"read": true}), text_reply("two")],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 32_768).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    assert!(windows(&pool, provider).await.is_empty(), "turn 1 streamed no event");

    run_turn(&pool, conversation.id, hello()).await.expect("turn 2 compacts and runs");

    assert_eq!(super::compaction::dividers(&pool, conversation.id).await, 1, "it compacted");
    assert_eq!(windows(&pool, provider).await.len(), 2, "the summary's reading");
}

/// Writes what a `fmt` layer logs into a shared buffer.
#[derive(Clone, Default)]
struct Logged(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logged {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_api_serves_numbers_only(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("usage-marker", vec![vec![json!({"read": true}), json!({"emit": [planted_event()]}), text_reply("hi")]]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    let logged = Logged::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer({
            let logged = logged.clone();
            move || logged.clone()
        })
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);

    {
        use tracing::instrument::WithSubscriber;
        let _default = tracing::dispatcher::set_default(&dispatch);
        run_turn(&pool, conversation.id, hello()).with_subscriber(dispatch.clone()).await.expect("the turn runs");
    }

    let marker = MARKER.to_lowercase();
    let served = crate::api::usage_windows::provider_usage(&pool, provider).await.expect("served");
    let served = serde_json::to_string(&served).expect("serialize");
    assert!(served.contains("\"five_hour\"") && served.contains("0.45"), "the numbers are served: {served}");
    assert!(!served.to_lowercase().contains(&marker), "{served}");
    let rows: Vec<Value> = sqlx::query_scalar("SELECT row_to_json(w) FROM provider_usage_windows w").fetch_all(&pool).await.expect("rows");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(!Value::Array(rows.clone()).to_string().to_lowercase().contains(&marker), "{rows:?}");
    let logs = String::from_utf8(logged.0.lock().unwrap_or_else(|e| e.into_inner()).clone()).expect("utf-8");
    // sqlx logs each statement: the capture covered the record itself.
    assert!(logs.contains("INSERT INTO provider_usage_windows"), "the capture saw the turn's record: {logs}");
    assert!(!logs.to_lowercase().contains(&marker), "{logs}");
}

/// One API call's `lines` with `input`, cache `read` and cache `write`
/// tokens on its `message_start`, and the planted marker as every line's
/// session id.
fn call_with_usage(mut lines: Vec<Value>, input: i64, read: i64, write: i64) -> Vec<Value> {
    lines[0]["event"]["message"]["usage"] =
        json!({"input_tokens": input, "cache_read_input_tokens": read, "cache_creation_input_tokens": write, "output_tokens": 1});
    for line in &mut lines {
        line["session_id"] = json!(format!("{MARKER}-session"));
    }
    lines
}

/// T11 (SME-486, retro P3): the turn span carries its Claude Code calls'
/// input, cache read and cache write tokens, summed over the turn's calls
/// and recorded once; never the session id.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turns_span_carries_its_claude_code_calls_tokens(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let tool = crate::claude_cli::tests::fake::tool_block("toolu_t11", "todoread", &json!({}));
    let first = call_with_usage(api_call(0, vec![tool], "tool_use", 5), 100, 2_000, 300);
    let mut second = call_with_usage(api_call(0, vec![text_block("done")], "end_turn", 3), 40, 2_300, 50);
    second.push(crate::claude_cli::tests::fake::result_ok());
    let call = json!({"call": [{"id": "toolu_t11", "name": "mcp__smelt__todoread", "input": {}}]});
    let fake = FakeCli::new("usage-span", vec![vec![json!({"read": true}), json!({"emit": first}), call, json!({"emit": second})]]);
    fake.pick_for(&pool, conversation.id).await;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn runs");

    let turns: Vec<_> = spans.iter().filter(|span| span.name == "turn").collect();
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    let attribute = |key: &str| -> Vec<opentelemetry::Value> {
        turn.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| kv.value.clone()).collect()
    };
    assert_eq!(attribute("turn.steps"), vec![2_i64.into()], "two calls");
    assert_eq!(attribute("gen_ai.usage.input_tokens"), vec![140_i64.into()], "{turn:?}");
    assert_eq!(attribute("gen_ai.usage.cache_read.input_tokens"), vec![4_300_i64.into()], "{turn:?}");
    assert_eq!(attribute("gen_ai.usage.cache_creation.input_tokens"), vec![350_i64.into()], "{turn:?}");
    let shown = format!("{spans:?}").to_lowercase();
    assert!(!shown.contains(&MARKER.to_lowercase()), "no session id on a span: {shown}");
}

/// Round 1 A-L2: the turn span's tokens count every Claude Code call of
/// the turn, as `model_call_usage` does: the compaction's summary
/// (`once`) too, not only the turn's own calls. Recorded once.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turns_span_counts_its_compactions_summary_call(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let tool = crate::claude_cli::tests::fake::tool_block("toolu_al2", "todoread", &json!({}));
    let first = call_with_usage(api_call(0, vec![tool], "tool_use", 5), 50_000, 1, 2);
    let call = json!({"call": [{"id": "toolu_al2", "name": "mcp__smelt__todoread", "input": {}}]});
    let reply = |text: &str, input, read, write| {
        let mut lines = call_with_usage(api_call(0, vec![text_block(text)], "end_turn", 3), input, read, write);
        lines.push(crate::claude_cli::tests::fake::result_ok());
        json!({"emit": lines})
    };
    let fake = FakeCli::new("usage-span-compaction", vec![
        vec![json!({"read": true}), json!({"emit": first}), call],
        vec![json!({"read": true}), reply("SUMMARY-AL2", 7, 8, 9)],
        vec![json!({"read": true}), reply("after", 10, 20, 30)],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 65_536).await;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn compacts and carries on");

    assert_eq!(super::compaction::dividers(&pool, conversation.id).await, 1, "it compacted");
    let turns: Vec<_> = spans.iter().filter(|span| span.name == "turn").collect();
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    let attribute = |key: &str| -> Vec<opentelemetry::Value> {
        turn.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| kv.value.clone()).collect()
    };
    assert_eq!(attribute("gen_ai.usage.input_tokens"), vec![50_017_i64.into()], "{turn:?}");
    assert_eq!(attribute("gen_ai.usage.cache_read.input_tokens"), vec![29_i64.into()], "{turn:?}");
    assert_eq!(attribute("gen_ai.usage.cache_creation.input_tokens"), vec![41_i64.into()], "{turn:?}");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_conversation_route_is_none_off_claude_code(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let other = db::create_conversation(&pool).await.expect("another conversation");
    start_mock_upstream(&pool, vec![text_reply_body("hi")]).await;
    let fake = FakeCli::new("usage-route", vec![]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    let http_provider = crate::providers::conversation_model(&pool, other.id)
        .await
        .expect("read")
        .as_ref()
        .and_then(crate::providers::ConversationModel::choice)
        .map(|c| c.provider_id)
        .expect("the HTTP provider is the default");
    let row = db::ProviderUsageWindowRow {
        window_name: "five_hour".into(),
        utilization: 0.45,
        status: "allowed".into(),
        resets_at: None,
        seen_at: chrono::Utc::now(),
    };
    db::record_usage_windows(&pool, provider, std::slice::from_ref(&row)).await.expect("record");
    // A row on the HTTP provider too, so only the kind check can say None.
    db::record_usage_windows(&pool, http_provider, std::slice::from_ref(&row)).await.expect("record");

    let route = crate::api::usage_windows::conversation_usage_windows;
    assert_eq!(route(&pool, other.id).await.expect("read"), None, "an HTTP provider's conversation");
    let usage = route(&pool, conversation.id).await.expect("read").expect("a Claude Code conversation's");
    assert_eq!((usage.provider_id, usage.windows.len()), (provider, 1));
    assert!(crate::api::usage_windows::provider_usage(&pool, -1).await.is_err(), "a provider that doesn't exist");
}
