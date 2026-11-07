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
    let close = replies(&calls, &held, None).expect("every call has a result");
    assert_eq!(close.replies.len(), 2, "only the reply's calls");
    assert_eq!(reply_of(&close.replies, "toolu_a"), ("[]".to_string(), false));
    assert_eq!(reply_of(&close.replies, "toolu_b"), ("boom".to_string(), true));
    assert_eq!(close.placeholder, None);
}

#[test]
fn test_a_question_with_no_result_gets_its_placeholder() {
    let asked = replies(&[expected("toolu_q", crate::questions::ASK_USER)], &[], Some("toolu_q")).expect("the asked question has a placeholder");
    assert_eq!(reply_of(&asked.replies, "toolu_q"), (ASKED_USER.to_string(), false));
    assert_eq!(asked.placeholder.as_deref(), Some("toolu_q"));
    let parent = replies(&[expected("toolu_p", crate::agents::ASK_PARENT)], &[], Some("toolu_p")).expect("the asked question has a placeholder");
    assert_eq!(reply_of(&parent.replies, "toolu_p"), (ASKED_PARENT.to_string(), false));
    assert_eq!(parent.placeholder.as_deref(), Some("toolu_p"));
}

/// PR #295 review 1 M1: a question the turn never recorded as asked has
/// no placeholder, so the run isn't closed and the call reads stopped.
#[test]
fn test_a_question_never_asked_closes_nothing() {
    let calls = [expected("toolu_q", crate::questions::ASK_USER)];
    assert!(replies(&calls, &[], None).is_none(), "nothing asked");
    assert!(replies(&calls, &[], Some("toolu_other")).is_none(), "another call asked");
}

#[test]
fn test_a_call_missing_its_result_closes_nothing() {
    let calls = [expected("toolu_q", crate::questions::ASK_USER), expected("toolu_a", "todoread")];
    assert!(replies(&calls, &[], Some("toolu_q")).is_none(), "todoread has no result");
}

/// A refused question's held result wins over the placeholder, and isn't
/// a placeholder: it's delivered like any result (PR #295 review 1 L2).
#[test]
fn test_a_refused_questions_result_wins_over_the_placeholder() {
    let calls = [expected("toolu_q", crate::questions::ASK_USER), expected("toolu_again", crate::questions::ASK_USER)];
    let held = [result("toolu_again", "ask one set of questions at a time", Some(true))];
    let close = replies(&calls, &held, Some("toolu_q")).expect("a reply for each");
    assert_eq!(reply_of(&close.replies, "toolu_again"), ("ask one set of questions at a time".to_string(), true));
    assert_eq!(reply_of(&close.replies, "toolu_q"), (ASKED_USER.to_string(), false));
    assert_eq!(close.placeholder.as_deref(), Some("toolu_q"), "only the asked one");
}
