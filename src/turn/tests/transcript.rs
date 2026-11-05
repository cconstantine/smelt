//! SME-386 F11: the summariser always sees the user's ask, and the last
//! summary, however much of the turn's tail the transcript has to cut.

use super::ask_user::tool_calls_body;
use super::window_stop::set_context_window;
use super::*;

fn text_message(id: i64, role: &str, text: &str) -> Message {
    message_with_blocks(id, role, vec![text_block(text)])
}

fn tool_call(id: i64) -> Message {
    message_with_blocks(
        id,
        "assistant",
        vec![anthropic::ContentBlock::ToolUse { id: format!("t{id}"), name: "seq".to_string(), input: serde_json::json!({}) }],
    )
}

fn tool_result(id: i64, content: &str) -> Message {
    message_with_blocks(
        id,
        "user",
        vec![anthropic::ContentBlock::ToolResult { tool_use_id: format!("t{}", id - 1), content: content.to_string(), is_error: None }],
    )
}

/// F11's shape: one ask, then steps that read big outputs.
fn long_turn(ask: &str, steps: i64) -> Vec<Message> {
    let mut messages = vec![text_message(1, "user", ask)];
    for step in 0..steps {
        messages.push(tool_call(2 + step * 2));
        messages.push(tool_result(3 + step * 2, &format!("output {step} {}", "x".repeat(2_000))));
    }
    messages
}

/// `messages` compacted once with `summary`, as `compact` saves it.
fn with_summary(mut messages: Vec<Message>, summary: &str) -> Vec<Message> {
    let last = messages.last().map_or(0, |m| m.id);
    for (offset, (role, blocks)) in compaction_messages(summary.to_string(), last, &Typed::default(), &[], None).into_iter().enumerate() {
        messages.push(message_with_blocks(last + 1 + offset as i64, role, blocks));
    }
    messages
}

#[test]
fn test_a_first_compaction_cut_to_its_budget_keeps_the_ask() {
    let ask = "Do steps one to nine, then stop and tell me.";
    let transcript = compaction_transcript(&long_turn(ask, 30), 5_000);
    assert!(transcript.contains(ask), "{transcript}");
    assert!(transcript.contains("output 29"), "the latest work stays");
    assert!(!transcript.contains("output 0 "), "the oldest work is what's dropped");
    assert!(transcript.contains("omitted"), "the cut is marked");
}

#[test]
fn test_a_later_compaction_cut_to_its_budget_keeps_the_ask_and_the_last_summary() {
    let ask = "Do steps one to nine, then stop and tell me.";
    let mut messages = with_summary(long_turn(ask, 2), "Steps one and two are done.");
    let next = messages.last().map_or(0, |m| m.id) + 1;
    for step in 0..30 {
        messages.push(tool_call(next + step * 2));
        messages.push(tool_result(next + 1 + step * 2, &format!("later {step} {}", "x".repeat(2_000))));
    }
    let transcript = compaction_transcript(&messages, 5_000);
    assert!(transcript.contains(ask), "{transcript}");
    assert!(transcript.contains("Steps one and two are done."), "{transcript}");
    assert!(transcript.contains("later 29"), "the latest work stays");
    assert!(!transcript.contains("later 0 "), "the oldest work is what's dropped");
}

/// A later compaction has the ask quoted even when nothing is cut: a summary
/// can paraphrase it away (F10's "then stop"), and it's before the boundary.
#[test]
fn test_a_later_compaction_that_fits_still_has_the_ask_word_for_word() {
    let ask = "Do step one, then stop and tell me.";
    let messages = with_summary(long_turn(ask, 1), "Working through the steps.");
    let transcript = compaction_transcript(&messages, 100_000);
    assert!(transcript.contains(ask), "{transcript}");
    assert!(transcript.contains("requests, not a record"), "an ask isn't to be read as done: {transcript}");
    assert!(!transcript.contains("output 0 "), "what the summary replaced stays out");
}

/// A first compaction that fits has everything word for word already.
#[test]
fn test_a_first_compaction_that_fits_is_sent_as_it_is() {
    let transcript = compaction_transcript(&long_turn("The ask.", 1), 100_000);
    assert_eq!(transcript.matches("The ask.").count(), 1, "{transcript}");
    assert!(transcript.starts_with("user: The ask."), "{transcript}");
}

/// A huge paste nobody has answered yet: its start is usually the ask, and
/// the tail keeps only its end.
#[test]
fn test_a_huge_unanswered_message_keeps_its_start() {
    let paste = format!("Please review this. {}", "y".repeat(100_000));
    let transcript = compaction_transcript(&[text_message(1, "user", &paste)], 5_000);
    assert!(transcript.contains("Please review this."), "{}", &transcript[..200]);
    assert!(transcript.chars().count() <= 5_000);
}

/// What the user stopped was called off, so it isn't an ask to keep.
#[test]
fn test_a_stopped_request_isnt_kept_as_an_ask() {
    let mut messages = long_turn("The real ask.", 1);
    messages.push(text_message(4, "assistant", "Done with that."));
    messages.push(text_message(5, "user", "Delete everything."));
    messages.push(text_message(6, "user", &crate::api::chat::mark_notice(STOP_NOTICE)));
    let messages = with_summary(messages, "The summary.");
    let transcript = compaction_transcript(&messages, 100_000);
    assert!(transcript.contains("The real ask."), "{transcript}");
    assert!(!transcript.contains("Delete everything."), "{transcript}");
}

/// A notice isn't the user's ask.
#[test]
fn test_smelts_own_notices_arent_kept_as_asks() {
    let mut messages = long_turn("The real ask.", 1);
    messages.push(text_message(4, "user", &crate::api::chat::mark_notice("Terminal command c1 finished: exit code 0.")));
    let messages = with_summary(messages, "The summary.");
    let transcript = compaction_transcript(&messages, 100_000);
    assert!(!transcript.contains("c1 finished"), "{transcript}");
}

/// The budget is what the summarization call was sized for, quoted words,
/// summary, heading and all.
#[test]
fn test_the_budget_holds_at_any_size() {
    let long_ask = "a".repeat(30_000);
    let later = "b".repeat(30_000);
    let summary = "s".repeat(30_000);
    let mut messages = long_turn(&long_ask, 40);
    let next = messages.last().map_or(0, |m| m.id) + 1;
    messages.push(text_message(next, "assistant", "Reply."));
    messages.push(text_message(next + 1, "user", &later));
    let messages = with_summary(messages, &summary);
    for max in [0, 1, 10, 31, 100, 500, 1_000, 5_000, 20_000, 59_424, 200_000] {
        let transcript = compaction_transcript(&messages, max);
        assert!(transcript.chars().count() <= max, "{} chars at {max}", transcript.chars().count());
    }
    // Multi-byte text counts in characters, as the budget does, and doesn't
    // split one.
    let wide = with_summary(long_turn(&"é".repeat(30_000), 40), &"ü".repeat(30_000));
    let transcript = compaction_transcript(&wide, 5_000);
    assert!(transcript.chars().count() <= 5_000);
    assert!(transcript.contains('é') && transcript.contains('ü'));
}

/// The ask and the summary leave the tail most of the budget, so the latest
/// work isn't crowded out at a real window's size.
#[test]
fn test_the_tail_gets_at_least_half() {
    let long_ask = "a".repeat(30_000);
    let messages = with_summary(long_turn(&long_ask, 2), &"s".repeat(30_000));
    let next = messages.last().map_or(0, |m| m.id) + 1;
    let mut messages = messages;
    for step in 0..30 {
        messages.push(tool_call(next + step * 2));
        messages.push(tool_result(next + 1 + step * 2, &"z".repeat(2_000)));
    }
    let transcript = compaction_transcript(&messages, 20_000);
    assert!(transcript.matches('z').count() >= 9_000, "{} of the tail kept", transcript.matches('z').count());
}

/// The real caller: what actually reaches the summary request.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_summary_request_carries_the_ask_past_the_cut(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let step = with_usage(&tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]), 30_000, 0, 100);
    let requests = start_recording_mock_upstream(&pool, vec![step, text_reply_body("The summary."), text_reply_body("Done.")]).await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let ask = "Do steps one to nine, then stop and tell me.";
    db::create_message(&pool, conversation.id, "user", &[text_block(ask)]).await.expect("the ask");
    // More than the 95,424 characters a 40,000-token window's summary takes.
    db::create_message(&pool, conversation.id, "assistant", &[text_block(&"x".repeat(150_000))]).await.expect("a long reply");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends");

    let requests = requests.lock().expect("the request log");
    let summary = requests.iter().find(|r| r["system"].to_string().contains("You are compacting")).expect("a summary request");
    let sent = summary["messages"][0]["content"][0]["text"].as_str().expect("its text");
    assert!(sent.contains(ask), "{}", &sent[..300]);
    assert!(sent.chars().count() <= 95_424 + 200, "{} chars", sent.chars().count());
}
