//! SME-286: a tool call the model wrote as text gets a correction, and
//! the turn carries on; corrections are bounded.

use super::*;

/// A content block of a mock reply.
enum Block<'a> {
    Thinking(&'a str),
    Text(&'a str),
    /// A `todoread` call (a cheap tool that needs only the database).
    Todoread(&'a str),
}

/// A reply of `blocks` ending with `stop_reason`, as SSE, its text
/// escaped by `serde_json` (`text_reply_body` pastes text into JSON raw,
/// so it can't carry markup's quotes and newlines).
fn text_blocks_body(blocks: &[Block], stop_reason: &str) -> String {
    use serde_json::json;
    let mut events = vec![json!({"type": "message_start", "message": {"usage": {
        "input_tokens": 100, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 1}}})];
    for (index, block) in blocks.iter().enumerate() {
        let (start, delta) = match block {
            Block::Thinking(t) => (json!({"type": "thinking", "thinking": ""}), json!({"type": "thinking_delta", "thinking": t})),
            Block::Text(t) => (json!({"type": "text", "text": ""}), json!({"type": "text_delta", "text": t})),
            Block::Todoread(id) => (
                json!({"type": "tool_use", "id": id, "name": "todoread", "input": {}}),
                json!({"type": "input_json_delta", "partial_json": "{}"}),
            ),
        };
        events.push(json!({"type": "content_block_start", "index": index, "content_block": start}));
        events.push(json!({"type": "content_block_delta", "index": index, "delta": delta}));
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 60}}));
    events.push(json!({"type": "message_stop"}));
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

/// The evidence: conversation 2294639360's message 98, prose and then a
/// call written as text, which ended the turn.
const EVIDENCE: &str = include_str!("../../text_calls/fixtures/evidence_98.txt");
const EVIDENCE_TOOL: &str = "mcp__linear.app__add_issue_comment";

fn markup_reply() -> String {
    text_blocks_body(&[Block::Thinking("I'll comment."), Block::Text(EVIDENCE)], "end_turn")
}

fn call_reply(id: &str) -> String {
    text_blocks_body(&[Block::Todoread(id)], "tool_use")
}

fn roles(saved: &[crate::models::Message]) -> Vec<&str> {
    saved.iter().map(|m| m.role.as_str()).collect()
}

/// The text of a message that is one text block, or `None`.
fn notice_text(message: &crate::models::Message) -> Option<String> {
    match message.blocks().expect("blocks").as_slice() {
        [anthropic::ContentBlock::Text { text }] => Some(text.clone()),
        _ => None,
    }
}

/// How many saved messages are text-call notices.
fn notices(saved: &[crate::models::Message]) -> usize {
    saved
        .iter()
        .filter(|m| notice_text(m).and_then(|t| crate::text_calls::parse_text_call_notice(&t)).is_some())
        .count()
}

/// The last message request `n` sent: its role and its first text.
fn last_sent(requests: &std::sync::Mutex<Vec<serde_json::Value>>, n: usize) -> (String, String) {
    let log = requests.lock().expect("the request log");
    let last = log[n]["messages"].as_array().expect("messages").last().expect("a message").clone();
    let text = last["content"][0]["text"].as_str().unwrap_or_default().to_string();
    (last["role"].as_str().expect("a role").to_string(), text)
}

/// A1: a reply that only writes a call as text gets the correction, and
/// the model's next, real call runs.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_written_as_text_gets_a_correction_and_the_turn_goes_on(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests =
        start_recording_mock_upstream(&pool, vec![markup_reply(), call_reply("toolu_real"), text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(roles(&saved), ["user", "assistant", "user", "assistant", "user", "assistant"], "{saved:?}");
    let notice = notice_text(&saved[2]).expect("the notice is one text block");
    assert_eq!(crate::text_calls::parse_text_call_notice(&notice), Some(vec![Some(EVIDENCE_TOOL.to_string())]), "{notice}");
    assert_eq!(requests.lock().expect("the request log").len(), 3);
    assert_eq!(last_sent(&requests, 1), ("user".to_string(), notice), "the model reads the notice next");
}

/// A3: after 3 corrections in a row, the 4th call written as text ends
/// the turn with an error naming the tool, and every tab is told.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_corrections_are_bounded_and_end_in_a_visible_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    // Ids no other test's database hands out: what this test hears on its
    // conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![markup_reply()]).await;

    // Bounded at 10 model calls, so a missing bound fails as 10, not a hang.
    let error = run_turn_bounded(&pool, TurnInFlight::start(conversation.id), Some(hello()), 10, None, false)
        .await
        .expect_err("the turn fails once corrections run out");

    assert_eq!(requests.lock().expect("the request log").len(), 4);
    let error = error.to_string();
    assert!(error.contains("4 times in a row") && error.contains(EVIDENCE_TOOL), "{error}");
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    assert_eq!(saved.iter().filter(|m| m.role == "assistant").count(), 4, "{saved:?}");
    assert_eq!(notices(&saved), 4, "the last notice is saved too, for the next turn");

    // A sent turn's failure reaches every tab, as any turn failure does.
    let sent = db::create_conversation(&pool).await.expect("create conversation");
    let mut rx = events::subscribe(sent.id);
    start_turn(pool.clone(), sent.id, "hi".to_string()).await.expect("send");
    let published = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::TurnError { message }) => return message,
                Ok(_) => continue,
                Err(e) => panic!("the event stream ended: {e}"),
            }
        }
    })
    .await
    .expect("a TurnError within 10 s");
    assert!(published.contains("4 times in a row") && published.contains(EVIDENCE_TOOL), "{published}");
}

/// A4: a real call resets the streak: three corrections after it don't
/// reach the bound.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_real_call_resets_the_streak(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let bodies = vec![
        markup_reply(),
        call_reply("toolu_reset"),
        markup_reply(),
        markup_reply(),
        markup_reply(),
        text_reply_body("Done."),
    ];
    let requests = start_recording_mock_upstream(&pool, bodies).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(requests.lock().expect("the request log").len(), 6);
    assert_eq!(notices(&saved), 4, "{saved:?}");
}

/// A5: each form the chat templates produce gets the correction through
/// the turn, not only through `find`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_every_form_gets_the_correction(pool: PgPool) {
    let _guard = lock_turn_tests();
    let forms = [
        include_str!("../../text_calls/fixtures/qwen3_coder.txt"),
        include_str!("../../text_calls/fixtures/hermes.txt"),
        include_str!("../../text_calls/fixtures/glm.txt"),
        include_str!("../../text_calls/fixtures/minimax.txt"),
        include_str!("../../text_calls/fixtures/seed_oss.txt"),
        include_str!("../../text_calls/fixtures/mistral.txt"),
        include_str!("../../text_calls/fixtures/deepseek_v3_1.txt"),
        include_str!("../../text_calls/fixtures/kimi_k2.txt"),
        include_str!("../../text_calls/fixtures/gemma4.txt"),
        include_str!("../../text_calls/fixtures/lfm2.txt"),
        include_str!("../../text_calls/fixtures/gpt_oss.txt"),
        include_str!("../../text_calls/fixtures/functionary_v3_1.txt"),
        include_str!("../../text_calls/fixtures/deepseek_v3_2_dsml.txt"),
        include_str!("../../text_calls/fixtures/evidence_96.txt"),
    ];
    for form in forms {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let body = text_blocks_body(&[Block::Text(form)], "end_turn");
        let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Done.")]).await;
        let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
        assert_eq!(notices(&saved), 1, "{form:?}: {saved:?}");
        assert_eq!(requests.lock().expect("the request log").len(), 2, "{form:?}");
    }
}

/// A6: a reply that quotes markup in code is an ordinary answer: no
/// notice, and the turn ends.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_quoted_markup_ends_the_turn_as_before(pool: PgPool) {
    let _guard = lock_turn_tests();
    let qwen = include_str!("../../text_calls/fixtures/qwen3_coder.txt");
    let fenced = format!("A Qwen call looks like this:\n\n```xml\n{qwen}\n```");
    let inline = "Mistral starts its calls with `[TOOL_CALLS]get_time[ARGS]`.".to_string();
    for text in [fenced, inline] {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let body = text_blocks_body(&[Block::Text(&text)], "end_turn");
        let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Again?")]).await;
        let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
        assert_eq!(roles(&saved), ["user", "assistant"], "{text:?}");
        assert_eq!(requests.lock().expect("the request log").len(), 1, "{text:?}");
    }
}

/// A2: a reply that makes a real call and also writes one as text (the
/// evidence's messages 92 and 96) runs the real one and gets the
/// correction after its result, so the model doesn't think the written
/// one ran.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_mixed_reply_gets_the_correction_after_its_results(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let written = include_str!("../../text_calls/fixtures/evidence_96.txt");
    let mixed = text_blocks_body(
        &[Block::Text("Now verify."), Block::Todoread("toolu_mixed"), Block::Text(written)],
        "tool_use",
    );
    let requests = start_recording_mock_upstream(&pool, vec![mixed, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(roles(&saved), ["user", "assistant", "user", "user", "assistant"], "{saved:?}");
    assert!(matches!(saved[2].blocks().expect("blocks").as_slice(), [anthropic::ContentBlock::ToolResult { .. }]), "{saved:?}");
    let notice = notice_text(&saved[3]).expect("the notice is one text block");
    assert_eq!(
        crate::text_calls::parse_text_call_notice(&notice),
        Some(vec![Some("mcp__linear.app__issue_read".to_string())])
    );
    // What the model reads next: the call's result, then the notice.
    let log = requests.lock().expect("the request log");
    let sent: Vec<serde_json::Value> = log[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .skip(2)
        .flat_map(|m| m["content"].as_array().expect("content").clone())
        .collect();
    let types: Vec<&str> = sent.iter().map(|b| b["type"].as_str().expect("a type")).collect();
    assert_eq!(types, ["tool_result", "text"], "{sent:?}");
    assert_eq!(sent[1]["text"], notice.as_str());
}

/// A7: a `stop_reason` of `tool_use` with no call in the reply is the
/// reply's end, not an empty set of results: with a call written as text
/// it gets the correction; without, the turn ends. No empty message is
/// saved either way (one was, and the next request sent it).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_tool_use_with_no_calls_is_the_end_of_the_reply(pool: PgPool) {
    let _guard = lock_turn_tests();
    let written = text_blocks_body(&[Block::Text(EVIDENCE)], "tool_use");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![written, text_reply_body("Done.")]).await;
    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
    assert_eq!(roles(&saved), ["user", "assistant", "user", "assistant"], "{saved:?}");
    assert_eq!(notices(&saved), 1, "{saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 2);

    let plain = text_blocks_body(&[Block::Text("Let me check.")], "tool_use");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![plain, text_reply_body("Done.")]).await;
    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
    assert_eq!(roles(&saved), ["user", "assistant"], "{saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 1);
    let all = db::list_messages(&pool, conversation.id).await.expect("list messages");
    assert!(all.iter().all(|m| m.content != "[]"), "no empty message is saved: {all:?}");
}

/// A8: a reply with no text and no call (thinking only, as after the
/// evidence's compaction, or empty) gets one nudge, and the model carries
/// on; a second such reply ends the turn, as a model that meant to stop.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_with_no_text_and_no_call_gets_one_nudge(pool: PgPool) {
    use crate::text_calls::EMPTY_REPLY_NOTICE;
    let _guard = lock_turn_tests();
    let thinking = || text_blocks_body(&[Block::Thinking("The context was compacted.")], "end_turn");
    let nudges = |saved: &[crate::models::Message]| saved.iter().filter(|m| notice_text(m).as_deref() == Some(EMPTY_REPLY_NOTICE)).count();

    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![thinking(), text_reply_body("Done.")]).await;
    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
    assert_eq!(roles(&saved), ["user", "assistant", "user", "assistant"], "{saved:?}");
    assert_eq!(notice_text(&saved[2]).as_deref(), Some(EMPTY_REPLY_NOTICE));
    assert_eq!(requests.lock().expect("the request log").len(), 2);
    assert_eq!(last_sent(&requests, 1), ("user".to_string(), EMPTY_REPLY_NOTICE.to_string()));

    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![thinking(), thinking(), text_reply_body("Never sent.")]).await;
    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a second empty reply ends the turn, not as an error");
    assert_eq!(nudges(&saved), 1, "{saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 2);

    // An empty reply isn't saved (B3), but is nudged all the same.
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_blocks_body(&[], "end_turn"), text_reply_body("Done.")]).await;
    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
    assert_eq!(roles(&saved), ["user", "user", "assistant"], "{saved:?}");
    assert_eq!(nudges(&saved), 1);
    assert_eq!(requests.lock().expect("the request log").len(), 2);
}

/// A8: the nudge is once a turn, not once in a row: a real call between
/// two empty replies doesn't earn a second.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_nudge_resets_after_a_real_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let thinking = || text_blocks_body(&[Block::Thinking("Hmm.")], "end_turn");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let bodies = vec![thinking(), call_reply("toolu_between"), thinking(), text_reply_body("Done.")];
    let requests = start_recording_mock_upstream(&pool, bodies).await;
    run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
    assert_eq!(requests.lock().expect("the request log").len(), 4, "a real call resets the nudge");
}

/// A9: a reply cut off at its budget with a call written in its text gets
/// only the cut-off notice, and the turn ends as before.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_reply_with_markup_gets_only_the_cut_off_notice(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = text_blocks_body(&[Block::Text(EVIDENCE)], "max_tokens");
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Again?")]).await;
    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");
    assert_eq!(roles(&saved), ["user", "assistant", "user"], "{saved:?}");
    let notice = notice_text(&saved[2]).expect("one text block");
    assert!(notice.starts_with(crate::api::chat::CUT_OFF_NOTICE_START), "{notice}");
    assert_eq!(requests.lock().expect("the request log").len(), 1);
}

/// Review 1, finding 1: a provider that parses a call but leaves its
/// markup in the text too gets no correction for that call: it ran, and
/// telling the model it didn't would have it make the call again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_markup_for_a_call_the_reply_also_made_gets_no_correction(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let markup = "<tool_call>\n<function=todoread>\n</function>\n</tool_call>";
    let both = text_blocks_body(&[Block::Text(markup), Block::Todoread("toolu_both")], "tool_use");
    let requests = start_recording_mock_upstream(&pool, vec![both, text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(roles(&saved), ["user", "assistant", "user", "assistant"], "{saved:?}");
    assert_eq!(notices(&saved), 0, "{saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 2);
}
