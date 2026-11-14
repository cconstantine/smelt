//! A call whose input isn't one JSON value on a Claude Code provider
//! (SME-520): the CLI refuses it locally and echoes its own error, which
//! smelt reads as a slip — the turn carries on, smelt's own error result
//! is the record, and the CLI's words (which quote the input) never are.

use super::*;

/// An `init` line saying the CLI can call `mcp__smelt__todoread`, as
/// `unknown_call.rs`'s.
fn cli_init() -> Value {
    json!({"type": "system", "subtype": "init", "session_id": "00000000-0000-4000-8000-000000000001",
           "tools": ["mcp__smelt__todoread"],
           "mcp_servers": [{"name": "smelt", "status": "connected"}]})
}

/// A `user` event echoing the CLI's own refusal of `id`, whose input was
/// `raw`: the shape and words CLI 2.1.318 printed for `{"a":1}{"a":1}`
/// from a fake Messages endpoint (the SME-520 hand check), quoting the
/// input as it does.
fn refusal(id: &str, raw: &str) -> Value {
    let len = raw.len();
    let content = format!(
        "<tool_use_error>InputValidationError: mcp__smelt__todoread was called with input that could not be parsed as JSON.\n\
         You sent (first {len} of {len} bytes): {raw}\n\
         Common causes: unescaped backslashes in file paths (use / or \\\\), unescaped control characters, or truncated output. \
         Retry with valid JSON.</tool_use_error>"
    );
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "content": content, "is_error": true, "tool_use_id": id}
    ]}, "tool_use_result": format!("InputValidationError: JSON parse failed ({len} bytes)")})
}

/// A `todoread` call's events whose input deltas are `raw`, unparsed.
fn raw_todoread(id: &str, raw: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "content_block": {"type": "tool_use", "id": id, "name": "mcp__smelt__todoread", "input": {}}}),
        json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": raw}}),
    ]
}

fn saved_json(saved: &[(String, Vec<anthropic::ContentBlock>)]) -> String {
    serde_json::to_string(saved).expect("saved messages serialize")
}

fn tool_result_for(saved: &[(String, Vec<anthropic::ContentBlock>)], id: &str) -> Option<String> {
    saved.iter().flat_map(|(_, blocks)| blocks.iter()).find_map(|block| match block {
        anthropic::ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == id => Some(content.clone()),
        _ => None,
    })
}

const MARKER: &str = "CLI-INPUT-MARKER-3e9d";

fn broken_input() -> String {
    format!(r#"{{"note":"{MARKER}"}}{{"note":"{MARKER}"}}"#)
}

/// Test 9: the CLI's echo of its own refusal of a handed name whose input
/// didn't parse carries the turn on in the same run; the saved result is
/// smelt's error, never the CLI's words or the input.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_clis_refusal_of_an_unparsed_call_carries_the_turn_on(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("unparsed-carries", vec![vec![
        json!({"read": true}),
        json!({"emit": [cli_init()]}),
        json!({"emit": api_call(10, vec![raw_todoread("toolu_broken", &broken_input())], "tool_use", 5)}),
        json!({"emit": [refusal("toolu_broken", &broken_input())]}),
        text_reply("Recovered."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn carries on");

    assert_eq!(fake.started(), 1, "the same run carried on, no restart");
    let saved = saved(&pool, conversation.id).await;
    assert!(saved.iter().any(|(role, blocks)| role == "assistant"
        && matches!(&blocks[..], [anthropic::ContentBlock::ToolUse { id, name, input }]
            if id == "toolu_broken" && name == "todoread" && input == &json!({}))), "{saved:?}");
    let result = tool_result_for(&saved, "toolu_broken").expect("the call's result");
    assert!(result.contains("trailing characters") && result.contains("did not run"), "{result}");
    assert!(saved.iter().any(|(role, blocks)| role == "assistant"
        && matches!(&blocks[..], [anthropic::ContentBlock::Text { text }] if text == "Recovered.")), "{saved:?}");
    let json = saved_json(&saved);
    assert!(!json.contains("InputValidationError") && !json.contains(MARKER), "{saved:?}");
}

/// Test 10: if the CLI makes the bridge call after all, it gets smelt's
/// error result, and nothing diverges.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_bridge_call_for_an_unparsed_call_gets_the_error(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("unparsed-called", vec![vec![
        json!({"read": true}),
        json!({"emit": [cli_init()]}),
        json!({"emit": api_call(10, vec![raw_todoread("toolu_broken", &broken_input())], "tool_use", 5)}),
        json!({"call": [{"id": "toolu_broken", "name": "mcp__smelt__todoread", "input": {}}]}),
        text_reply("Recovered."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("no divergence");

    assert_eq!(fake.started(), 1);
    let results = fake.results(0);
    assert_eq!(results.len(), 1, "{results:?}");
    let text = results[0]["text"].as_str().expect("its text");
    assert!(text.contains("did not run") && results[0]["is_error"] == json!(true), "{results:?}");
}

/// Test 11: a parsed call beside an unparsed one stays guarded — the
/// CLI answering it itself is still a divergence (SME-447).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_echo_for_a_parsed_call_still_diverges(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let blocks = vec![raw_todoread("toolu_broken", &broken_input()), raw_todoread("toolu_here", "{}")];
    let fake = FakeCli::new("unparsed-diverges", vec![vec![
        json!({"read": true}),
        json!({"emit": [cli_init()]}),
        json!({"emit": api_call(10, blocks, "tool_use", 5)}),
        json!({"emit": [refusal("toolu_broken", &broken_input()), refusal("toolu_here", "{}")]}),
        text_reply("Unrelated."),
    ]]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("diverged");
    assert!(error.to_string().contains("answered tool call toolu_here itself"), "{error}");
}
