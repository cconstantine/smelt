use super::*;

fn cut_off_reply_body(text: &str) -> String {
    text_reply_body(text).replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#)
}

/// SME-111: a reply that hits its `max_tokens` is kept, followed by a
/// notice every tab gets live, naming the limit and what bound it; a
/// reply that finishes gets none.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_reply_is_followed_by_a_notice(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![cut_off_reply_body("Half a tho"), text_reply_body("Done.")]).await;
    let mut events = crate::events::subscribe(conversation.id);

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a cut-off turn still succeeds");

    let notice = cut_off_notice(16_384, ReplyLimit::OutputCap, None);
    let texts: Vec<(String, String)> = saved
        .iter()
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect();
    assert_eq!(texts.len(), 3, "the message, the reply and the notice: {texts:?}");
    assert!(texts[1].1.contains("Half a tho"), "the reply is kept: {texts:?}");
    assert_eq!(texts[2].0, "user");
    let notice_text = match saved[2].blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(notice_text, notice);
    assert_eq!(
        crate::api::chat::parse_cut_off_notice(&notice_text),
        Some((16_384, ReplyLimit::OutputCap, None)),
        "an unsized Anthropic model's cap bound it"
    );
    let mut published = false;
    while let Ok(event) = events.try_recv() {
        if let crate::events::ConversationEvent::MessagesAppended { messages } = event {
            published |= messages.iter().any(|m| m.id == saved[2].id);
        }
    }
    assert!(published, "every tab is told");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a finished turn");
    assert_eq!(saved.len(), 2, "no notice after a finished reply");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 16_384);
}

/// A reply cut off at `max_tokens` (60 tokens written): `blocks` in
/// Anthropic's order, each a content block's SSE events.
fn cut_off_in_a_call_body(blocks: &[[String; 3]]) -> String {
    reply_ending_body(blocks, "max_tokens")
}

/// A reply of `blocks` (each a content block's SSE events) that ends with
/// `stop_reason`, 60 tokens written.
fn reply_ending_body(blocks: &[[String; 3]], stop_reason: &str) -> String {
    let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}"#.to_string();
    let end = format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}"}},"usage":{{"output_tokens":60}}}}"#);
    std::iter::once(&start)
        .chain(blocks.iter().flatten())
        .chain([&end, &r#"{"type":"message_stop"}"#.to_string()])
        .map(|data| format!("data: {data}\n\n"))
        .collect()
}

/// The roles and contents of the messages request `n` sent.
fn sent_messages(requests: &std::sync::Mutex<Vec<serde_json::Value>>, n: usize) -> Vec<(String, serde_json::Value)> {
    requests.lock().expect("the request log")[n]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| (m["role"].as_str().expect("a role").to_string(), m["content"].clone()))
        .collect()
}

/// B3 (SME-122): an empty reply isn't saved: the API refuses an empty
/// assistant message once it isn't the last, so every later turn failed.
/// Since SME-286 the first gets a nudge and the model is called again;
/// the blank reply after it ends the turn, saving nothing either.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_empty_reply_is_not_saved_and_the_next_turn_works(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let empty = reply_ending_body(&[], "end_turn");
    let blank = reply_ending_body(&[text_events(0, " ")], "end_turn");
    let requests = start_recording_mock_upstream(&pool, vec![empty, blank, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("an empty reply doesn't fail the turn");
    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "user"], "nothing saved for the empty or the blank reply, only the nudge: {saved:?}");
    assert_eq!(only_text(&saved[1]), crate::text_calls::EMPTY_REPLY_NOTICE);

    run_turn(&pool, conversation.id, hello()).await.expect("the next turn");
    let roles: Vec<String> = sent_messages(&requests, 2).into_iter().map(|(role, _)| role).collect();
    assert_eq!(roles, ["user", "user", "user"]);
}

/// B3 (SME-122): a conversation that already holds an empty or blank
/// assistant message (saved before the fix) recovers: the history leaves
/// it out.
#[test]
fn test_history_leaves_out_an_empty_or_blank_reply() {
    let text = |t: &str| anthropic::ContentBlock::Text { text: t.to_string() };
    let history = history_for_request_all(vec![
        message_with_blocks(1, "user", vec![text("hi")]),
        message_with_blocks(2, "assistant", vec![]),
        message_with_blocks(3, "user", vec![text("again")]),
        message_with_blocks(4, "assistant", vec![text(" \n")]),
        message_with_blocks(5, "user", vec![text("and?")]),
        message_with_blocks(6, "assistant", vec![text(""), text("sure")]),
    ])
    .expect("parses");
    let contents: Vec<(String, Vec<anthropic::ContentBlock>)> = history.into_iter().map(|m| (m.role, m.content)).collect();
    assert_eq!(
        contents,
        vec![
            ("user".to_string(), vec![text("hi")]),
            ("user".to_string(), vec![text("again")]),
            ("user".to_string(), vec![text("and?")]),
            ("assistant".to_string(), vec![text("sure")]),
        ]
    );
}

/// B3 (SME-122): a reply the provider refused partway keeps its text and
/// is followed by a notice saying so, not saved as if it had finished.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_reply_keeps_its_text_and_says_it_was_refused(pool: PgPool) {
    use crate::api::chat::refusal_notice;
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = reply_ending_body(&[text_events(0, "Here is how to")], "refusal");
    start_recording_mock_upstream(&pool, vec![body]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a refusal doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{saved:?}");
    assert_eq!(only_text(&saved[1]), "Here is how to");
    assert_eq!(only_text(&saved[2]), refusal_notice(None));
}

/// B3 (SME-122): a reply refused in the middle of a tool call drops the
/// call (its input may be cut short), as a cut-off does, instead of
/// failing the turn on the half-written input; with nothing else in the
/// reply, only the notice is saved.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_refused_in_a_call_drops_the_call(pool: PgPool) {
    use crate::api::chat::refusal_notice;
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = reply_ending_body(&[todowrite_events(0, "toolu_cut", CUT_TODOS)], "refusal");
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a refusal in a call doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "user"], "{saved:?}");
    assert_eq!(only_text(&saved[1]), refusal_notice(Some("todowrite")));
    run_turn(&pool, conversation.id, hello()).await.expect("the next turn is accepted");
    assert!(!requests.lock().expect("the request log")[1].to_string().contains("toolu_cut"), "the dropped call appears nowhere");
}

/// A text block at `index`, started, written and stopped.
fn text_events(index: usize, text: &str) -> [String; 3] {
    [
        format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"text","text":""}}}}"#),
        format!(r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"text_delta","text":"{text}"}}}}"#),
        format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    ]
}

/// A `todowrite` call at `index` with `input` (escaped for a JSON string)
/// as its input so far, started, written and stopped.
fn todowrite_events(index: usize, id: &str, input: &str) -> [String; 3] {
    [
        format!(r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"todowrite","input":{{}}}}}}"#),
        format!(r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":"{input}"}}}}"#),
        format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    ]
}

const WHOLE_TODOS: &str = r#"{\"todos\":[{\"content\":\"a\",\"status\":\"pending\"}]}"#;
const CUT_TODOS: &str = r#"{\"todos\":[{\"content\":\"a long"#;

fn only_text(message: &crate::models::Message) -> String {
    match message.blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    }
}

/// SME-126: a reply cut off at its limit in the middle of a tool call
/// keeps its text and gets the notice naming the call, which never runs;
/// its usage is recorded, and the next request sends the reply, the
/// notice and the next message, and is accepted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_cut_off_in_a_call_keeps_its_text_and_names_the_call(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[text_events(0, "Writing the list."), todowrite_events(1, "toolu_cut", CUT_TODOS)]);
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a call cut off doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{saved:?}");
    assert_eq!(only_text(&saved[1]), "Writing the list.", "the reply's text, without the call");
    assert_eq!(only_text(&saved[2]), cut_off_notice(16_384, ReplyLimit::OutputCap, Some("todowrite")));
    assert!(db::get_conversation_todos(&pool, conversation.id).await.expect("todos").is_empty(), "the cut call never ran");
    let output: Vec<i64> = sqlx::query_scalar("SELECT output_tokens FROM model_call_usage WHERE conversation_id = $1")
        .bind(conversation.id)
        .fetch_all(&pool)
        .await
        .expect("the recorded calls");
    assert_eq!(output, [60], "the cut-off call's usage is recorded");

    run_turn(&pool, conversation.id, hello()).await.expect("the next turn is accepted");
    let requests = requests.lock().expect("the request log");
    let sent: Vec<(String, serde_json::Value)> = requests[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| (m["role"].as_str().expect("a role").to_string(), m["content"].clone()))
        .collect();
    let roles: Vec<&str> = sent.iter().map(|(role, _)| role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "user"], "{sent:?}");
    assert_eq!(sent[1].1, serde_json::json!([{"type": "text", "text": "Writing the list."}]));
    assert!(sent[2].1.to_string().contains("in the middle of a call to `todowrite`"), "{sent:?}");
    assert!(!requests[1].to_string().contains("toolu_cut"), "the dropped call appears nowhere");
}

/// SME-126: complete calls before the cut one are kept, not run, and
/// each gets a saved "not run" result, sent right after the call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_whole_calls_before_a_cut_one_are_kept_with_a_not_run_result(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[todowrite_events(0, "toolu_whole", WHOLE_TODOS), todowrite_events(1, "toolu_cut", CUT_TODOS)]);
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a call cut off doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "user"], "the message, the reply, its results, the notice: {saved:?}");
    assert!(
        matches!(saved[1].blocks().expect("blocks").as_slice(), [anthropic::ContentBlock::ToolUse { id, .. }] if id == "toolu_whole"),
        "{saved:?}"
    );
    assert_eq!(
        saved[2].blocks().expect("blocks"),
        vec![anthropic::ContentBlock::ToolResult {
            tool_use_id: "toolu_whole".to_string(),
            content: CUT_OFF_TOOL_CALL.to_string(),
            is_error: Some(true),
        }]
    );
    assert!(crate::api::chat::parse_cut_off_notice(&only_text(&saved[3])).is_some(), "{saved:?}");
    assert!(db::get_conversation_todos(&pool, conversation.id).await.expect("todos").is_empty(), "neither call ran");

    run_turn(&pool, conversation.id, hello()).await.expect("the next turn is accepted");
    let requests = requests.lock().expect("the request log");
    let sent = requests[1]["messages"].as_array().expect("messages");
    assert_eq!(sent[2]["content"][0]["tool_use_id"], "toolu_whole", "the result follows the call: {sent:?}");
    assert_eq!(sent[2]["content"][0]["content"], CUT_OFF_TOOL_CALL);
}

/// SME-126 review 1: a cut-off reply's whole calls get their "not run"
/// results right after the reply is saved, so a failure after that (here
/// recording the call's usage) doesn't leave them to be answered as
/// stopped mid-run, which they weren't.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_not_run_results_are_saved_before_anything_that_can_fail(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[todowrite_events(0, "toolu_whole", WHOLE_TODOS), todowrite_events(1, "toolu_cut", CUT_TODOS)]);
    start_recording_mock_upstream(&pool, vec![body]).await;
    sqlx::query("ALTER TABLE model_call_usage RENAME TO model_call_usage_gone")
        .execute(&pool)
        .await
        .expect("make recording the call fail");

    run_turn(&pool, conversation.id, hello()).await.expect_err("recording the call fails the turn");

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "the message, the reply and its results: {saved:?}");
    assert_eq!(
        saved[2].blocks().expect("blocks"),
        vec![anthropic::ContentBlock::ToolResult {
            tool_use_id: "toolu_whole".to_string(),
            content: CUT_OFF_TOOL_CALL.to_string(),
            is_error: Some(true),
        }]
    );
}

/// SME-126: a reply that is nothing but the cut call saves no assistant
/// message (the API refuses an empty one); the notice follows the
/// user's message alone.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_that_is_only_the_cut_call_saves_just_the_notice(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = cut_off_in_a_call_body(&[todowrite_events(0, "toolu_cut", CUT_TODOS)]);
    start_recording_mock_upstream(&pool, vec![body]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a call cut off doesn't fail the turn");

    let roles: Vec<&str> = saved.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "user"], "{saved:?}");
    assert_eq!(only_text(&saved[1]), cut_off_notice(16_384, ReplyLimit::OutputCap, Some("todowrite")));
}

/// SME-111 review 2: a reply bound by an Anthropic model's own reported
/// cap, which "Max reply tokens" can't raise, says it's the model's own
/// maximum rather than telling the user to raise the setting.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_at_the_models_own_cap_doesnt_say_raise_it(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    start_recording_mock_upstream(&pool, vec![cut_off_reply_body("Half a tho")]).await;
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_max_output, max_output) SELECT id, $1, 8192, 64000 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("a reported cap below the user's");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("turn");

    let notice = match saved[2].blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(parse_cut_off_notice(&notice), Some((8_192, ReplyLimit::ModelMaximum, None)));
}

/// SME-111 review 1: a reply the context window stopped
/// (`model_context_window_exceeded`, Claude 4.5+) is cut off too, and the
/// notice says the conversation ran out of room, whatever bound the budget.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_the_window_stopped_gets_a_room_left_notice(pool: PgPool) {
    use crate::api::chat::{ReplyLimit, parse_cut_off_notice};
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    // Stopped by the window after 9,000 tokens, well short of its budget.
    let body = with_usage(&text_reply_body("Half a tho"), 100, 0, 9_000)
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"model_context_window_exceeded""#);
    start_recording_mock_upstream(&pool, vec![body]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn still succeeds");

    assert_eq!(saved.len(), 3, "the message, the reply and a notice: {saved:?}");
    let notice = match saved[2].blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => text.clone(),
        other => panic!("one text block: {other:?}"),
    };
    assert_eq!(
        parse_cut_off_notice(&notice),
        Some((9_000, ReplyLimit::RoomLeft, None)),
        "what it wrote, not the budget it didn't reach (review 2)"
    );
}

/// SME-111 review 1: a llama.cpp reply budget too small to think in
/// (4,096 or less) turns thinking off, said plainly, instead of sending a
/// reasoning budget of 0 while thinking is on.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_budget_too_small_to_think_in_turns_thinking_off(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    sqlx::query("UPDATE inference_providers SET kind = 'llama_cpp'")
        .execute(&pool)
        .await
        .expect("make the mock a llama.cpp server");
    sqlx::query("INSERT INTO provider_models (provider_id, model, reported_context_window, max_output) SELECT id, $1, 262144, 4096 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .execute(&pool)
        .await
        .expect("a small reply cap");
    run_turn(&pool, conversation.id, hello()).await.expect("turn");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 4096);
    assert!(requests[0].get("thinking").is_none(), "{}", requests[0]);
    assert_eq!(requests[0]["chat_template_kwargs"], serde_json::json!({"enable_thinking": false}));
}

/// A model on an Anthropic provider that nothing sized keeps the reply
/// budget every turn had: a Claude model refuses one above its own cap.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unsized_anthropic_models_reply_budget_stays_as_it_was(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn");
    let requests = requests.lock().expect("the request log");
    assert_eq!(requests[0]["max_tokens"], 16_384);
}
