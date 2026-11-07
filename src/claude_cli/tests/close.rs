//! What each of a reply's waiting calls is answered with when its turn
//! returns (SME-444): `run::close::replies`.

use crate::anthropic::ContentBlock;
use crate::claude_cli::run::close::replies;
use crate::claude_cli::run::{ASKED_PARENT, ASKED_USER, Expected};

fn expected(id: &str, name: &str) -> Expected {
    Expected { id: id.to_string(), name: name.to_string(), input: serde_json::json!({}) }
}

fn result(id: &str, content: &str, is_error: Option<bool>) -> ContentBlock {
    ContentBlock::ToolResult { tool_use_id: id.to_string(), content: content.to_string(), is_error }
}

/// (text, is_error) of `id`'s reply.
fn reply_of(replies: &std::collections::HashMap<String, crate::claude_cli::bridge::ToolReply>, id: &str) -> (String, bool) {
    let reply = replies.get(id).unwrap_or_else(|| panic!("a reply for {id}"));
    (reply.text.join("\n"), reply.is_error)
}

#[test]
fn test_each_held_result_answers_its_call_with_its_is_error() {
    let calls = [expected("toolu_a", "todoread"), expected("toolu_b", "run_command")];
    let held = [result("toolu_b", "boom", Some(true)), result("toolu_a", "[]", None), result("toolu_x", "not ours", None)];
    let replies = replies(&calls, &held).expect("every call has a result");
    assert_eq!(replies.len(), 2, "only the reply's calls");
    assert_eq!(reply_of(&replies, "toolu_a"), ("[]".to_string(), false));
    assert_eq!(reply_of(&replies, "toolu_b"), ("boom".to_string(), true));
}

#[test]
fn test_a_question_with_no_result_gets_its_placeholder() {
    let calls = [expected("toolu_q", crate::questions::ASK_USER), expected("toolu_p", crate::agents::ASK_PARENT)];
    let replies = replies(&calls, &[]).expect("questions have placeholders");
    assert_eq!(reply_of(&replies, "toolu_q"), (ASKED_USER.to_string(), false));
    assert_eq!(reply_of(&replies, "toolu_p"), (ASKED_PARENT.to_string(), false));
}

#[test]
fn test_a_call_missing_its_result_closes_nothing() {
    let calls = [expected("toolu_q", crate::questions::ASK_USER), expected("toolu_a", "todoread")];
    assert!(replies(&calls, &[]).is_none(), "todoread has no result");
}

#[test]
fn test_a_refused_questions_result_wins_over_the_placeholder() {
    let calls = [expected("toolu_q", crate::questions::ASK_USER)];
    let held = [result("toolu_q", "ask one set of questions at a time", Some(true))];
    let replies = replies(&calls, &held).expect("a reply");
    assert_eq!(reply_of(&replies, "toolu_q"), ("ask one set of questions at a time".to_string(), true));
}
