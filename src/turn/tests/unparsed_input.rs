//! SME-520: a tool call whose input isn't one JSON value is never run;
//! it gets an error result the model can retry from, and counts toward
//! the SME-307 bound.

use super::*;

/// A reply of calls `(id, name, raw input)`, each one block, ending with
/// `stop_reason`.
fn calls_body(calls: &[(&str, &str, &str)], stop_reason: &str) -> String {
    use serde_json::json;
    let mut events = vec![json!({"type": "message_start", "message": {"usage": {
        "input_tokens": 100, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}})];
    for (index, (id, name, raw)) in calls.iter().enumerate() {
        events.push(json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": raw}}));
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 60}}));
    events.push(json!({"type": "message_stop"}));
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

const MARKER: &str = "INPUT-MARKER-77c1";

/// A `todowrite` input sent twice, joined: a replayed call's shape.
fn joined_todos() -> String {
    let one = format!(r#"{{"todos":[{{"content":"{MARKER}","status":"pending"}}]}}"#);
    format!("{one}{one}")
}

fn broken_reply(id: &str) -> String {
    calls_body(&[(id, "todowrite", &joined_todos())], "tool_use")
}

/// The tool result for `id` among `saved`: its content and error flag.
fn result_for(saved: &[crate::models::Message], id: &str) -> Option<(String, Option<bool>)> {
    saved.iter().flat_map(|m| m.blocks().expect("blocks")).find_map(|block| match block {
        anthropic::ContentBlock::ToolResult { tool_use_id, content, is_error } if tool_use_id == id => Some((content, is_error)),
        _ => None,
    })
}

/// Test 5: the broken call isn't run and gets the error naming serde's
/// message; the good call beside it runs; the turn goes on to finish.
/// The input reaches neither the result nor the record.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unparsed_call_gets_an_error_and_the_turn_goes_on(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let joined = joined_todos();
    let first = calls_body(&[("toolu_broken", "todowrite", &joined), ("toolu_good", "todoread", "{}")], "tool_use");
    let requests = start_recording_mock_upstream(&pool, vec![first, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("an unparsed call doesn't fail the turn");

    assert_eq!(requests.lock().expect("the request log").len(), 2);
    let (broken, is_error) = result_for(&saved, "toolu_broken").expect("the broken call's result");
    let column = joined.len() / 2 + 1;
    let expected = crate::anthropic::stream::unparsed::call_result(&format!("trailing characters at line 1 column {column}"));
    assert_eq!((broken.as_str(), is_error), (expected.as_str(), Some(true)));
    let (good, is_error) = result_for(&saved, "toolu_good").expect("the good call's result");
    assert_eq!(is_error, None, "the good call ran: {good}");
    assert!(db::get_conversation_todos(&pool, conversation.id).await.expect("todos").is_empty(), "the broken call never ran");
    let call = saved[1].blocks().expect("blocks").into_iter().find_map(|b| match b {
        anthropic::ContentBlock::ToolUse { id, input, .. } if id == "toolu_broken" => Some(input),
        _ => None,
    });
    assert_eq!(call, Some(serde_json::json!({})), "kept with an empty input");
    assert!(saved.iter().all(|m| !m.content.contains(MARKER)), "the input reaches no record: {saved:?}");
}

/// Test 6: a reply of only broken calls is a correction: the 4th in a row
/// ends the turn with the bound error, its result saved first.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_unparsed_calls_count_toward_the_bound(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let bodies = (1..=4).map(|n| broken_reply(&format!("toolu_{n}"))).collect();
    let requests = start_recording_mock_upstream(&pool, bodies).await;

    let error = run_turn_bounded(&pool, TurnInFlight::start(conversation.id), Some(hello()), 10, None, false)
        .await
        .expect_err("the 4th reply of broken calls ends the turn");

    assert_eq!(requests.lock().expect("the request log").len(), 4);
    let error = error.to_string();
    assert!(error.contains("4 times in a row") && error.contains("`todowrite`"), "{error}");
    assert!(!error.contains(MARKER), "{error}");
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    let (last, _) = result_for(&saved, "toolu_4").expect("the 4th reply's result is saved");
    assert!(last.contains("did not run"), "{last}");
}

/// Test 6: a good call between broken ones resets the count.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_good_call_resets_the_unparsed_streak(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let good = calls_body(&[("toolu_ok", "todoread", "{}")], "tool_use");
    let mut bodies: Vec<String> = (1..=3).map(|n| broken_reply(&format!("toolu_a{n}"))).collect();
    bodies.push(good);
    bodies.extend((1..=3).map(|n| broken_reply(&format!("toolu_b{n}"))));
    bodies.push(text_reply_body("Done."));
    let requests = start_recording_mock_upstream(&pool, bodies).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the good call reset the streak");
    assert_eq!(requests.lock().expect("the request log").len(), 8);
}

/// Test 7: a cut-off reply's broken call, before its cut last call, gets
/// the "not run" result like any whole call there, and the notice follows.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_replys_unparsed_call_is_not_run(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let joined = joined_todos();
    let body = calls_body(&[("toolu_broken", "todowrite", &joined), ("toolu_cut", "todowrite", r#"{"todos":[{"#)], "max_tokens");
    start_recording_mock_upstream(&pool, vec![body, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a cut-off reply doesn't fail the turn");

    let (result, is_error) = result_for(&saved, "toolu_broken").expect("the broken call's result");
    assert_eq!((result.as_str(), is_error), (CUT_OFF_TOOL_CALL, Some(true)));
    let last = saved.last().expect("a notice").blocks().expect("blocks");
    assert!(
        matches!(last.as_slice(), [anthropic::ContentBlock::Text { text }] if crate::api::chat::parse_cut_off_notice(text).is_some()),
        "{saved:?}"
    );
    assert!(db::get_conversation_todos(&pool, conversation.id).await.expect("todos").is_empty());
}

/// Test 8: an `ask_user` whose input didn't parse gets the same error, not
/// `parse_questions`' words about its empty input, and asks nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unparsed_question_asks_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let raw = format!(r#"{{"questions":[{{"question":"{MARKER}?","header":"Q"}}]}}x"#);
    let ask = calls_body(&[("toolu_ask", crate::questions::ASK_USER, &raw)], "tool_use");
    let requests = start_recording_mock_upstream(&pool, vec![ask, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn goes on");

    assert_eq!(requests.lock().expect("the request log").len(), 2, "the turn didn't stop to wait");
    let (result, is_error) = result_for(&saved, "toolu_ask").expect("the question's result");
    assert!(result.contains("trailing characters") && result.contains("did not run"), "{result}");
    assert_eq!(is_error, Some(true));
    assert!(db::get_pending_question(&pool, conversation.id).await.expect("question").is_none(), "no card");
}
