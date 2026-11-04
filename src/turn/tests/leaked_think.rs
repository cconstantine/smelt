//! SME-290: a reply whose only text is leaked `<think>` reasoning gets
//! SME-286's nudge. `after_reply`'s `has_text` tests the text with the
//! shared `text_calls::mask_think` rule, so reasoning-only text decides
//! "said nothing" and the model is called again. The masking is
//! decision-only: the reply saves verbatim, an unclosed `<think>` is text
//! (the review-2 rule reused whole), and real text after `</think>` is
//! text. The mock-upstream builders come from `text_calls`' helpers.

use super::text_calls::{last_sent, notice_text, roles, text_blocks_body, Block};
use super::*;

/// The nudge as saved: SME-357's mark before SME-286's words (as A8).
fn marked_nudge() -> String {
    format!("{}{}", crate::api::chat::NOTICE_MARK, crate::text_calls::EMPTY_REPLY_NOTICE)
}

/// The bug's shape: one text block that is nothing but leaked reasoning.
fn leaked_reply() -> String {
    text_blocks_body(&[Block::Text("<think>\nI'll think here.\n</think>")], "end_turn")
}

fn nudges(saved: &[crate::models::Message]) -> usize {
    let marked = marked_nudge();
    saved.iter().filter(|m| notice_text(m).as_deref() == Some(marked.as_str())).count()
}

/// N1: a reply whose only text is leaked reasoning falls through to the
/// nudge and the model is called again — the turn no longer ends
/// silently. Without part B (`has_text` on masked text) it fails: the raw
/// block counts as text. Green only when the reply also saves verbatim —
/// the masking decides, it saves nothing rewritten.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_whose_only_text_is_leaked_reasoning_gets_the_nudge(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![leaked_reply(), text_reply_body("Done.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    let marked = marked_nudge();
    assert_eq!(roles(&saved), ["user", "assistant", "user", "assistant"], "{saved:?}");
    assert_eq!(notice_text(&saved[2]).as_deref(), Some(marked.as_str()), "the third message is the nudge: {saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 2, "the model is called again after the nudge");
    assert_eq!(last_sent(&requests, 1), ("user".to_string(), marked), "the model reads the nudge next");
    let reply = saved[1].blocks().expect("the assistant reply has blocks");
    assert!(
        matches!(reply.as_slice(), [anthropic::ContentBlock::Text { text }] if text == "<think>\nI'll think here.\n</think>"),
        "decision-only: the leaked reasoning saved verbatim: {reply:?}"
    );
}

/// N2: a second reasoning-only reply before a real call ends the turn
/// (the `streak.nudged` bound A8 pins, for this new shape): one nudge,
/// two model calls, no error. Without part B it fails: no nudge at all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_another_reasoning_only_reply_ends_the_turn(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![leaked_reply(), leaked_reply(), text_reply_body("Never sent.")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("a second reasoning-only reply ends the turn, not as an error");

    assert_eq!(nudges(&saved), 1, "{saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 2, "the third reply is never sent");
}

/// N3: real text after `</think>` is an answer — no nudge, one request
/// (the ticket's explicit requirement). The guard for part B: green
/// without it, and red under a mutant that masks an unclosed `<think>`
/// or blanks to the reply's end (SME-286 review 2's own bug).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_text_after_the_reasoning_is_no_nudge(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = text_blocks_body(&[Block::Text("<think>reasoning</think>\n\nThe answer is 42.")], "end_turn");
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Again?")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(roles(&saved), ["user", "assistant"], "text after `</think>` is an answer: {saved:?}");
    assert_eq!(nudges(&saved), 0, "{saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 1);
}

/// N4: an unclosed `<think>` counts as text — no nudge (the decision the
/// plan settled: the rule is reused whole, so a `<think>` quoted in prose
/// can never blank the rest of a reply; a stream truly cut inside its
/// reasoning reaches the cut-off path before `has_text`). Same mutant
/// relationship as N3: it pins the never-closed half of the rule.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unclosed_think_is_text(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let body = text_blocks_body(&[Block::Text("<think>never closed")], "end_turn");
    let requests = start_recording_mock_upstream(&pool, vec![body, text_reply_body("Again?")]).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn succeeds");

    assert_eq!(roles(&saved), ["user", "assistant"], "an unclosed `<think>` is text, the turn ends as text does: {saved:?}");
    assert_eq!(nudges(&saved), 0, "{saved:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 1);
}

/// SME-386 T1: a call by a name no tool has isn't a real call, so it
/// doesn't reset the nudge. Alternating one with a reasoning-only reply
/// used to loop to the turn's whole bound (the unknown call cleared
/// `nudged`, the nudge zeroed the correction count); now the second
/// reasoning-only reply ends the turn: one nudge, four model calls.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unknown_call_between_reasoning_only_replies_doesnt_reset_the_nudge(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let unknown = |id: &str| text_blocks_body(&[Block::Call(id, "todosote")], "tool_use");
    let bodies: Vec<String> = (0..6).flat_map(|i| [unknown(&format!("toolu_t1_{i}")), leaked_reply()]).collect();
    let requests = start_recording_mock_upstream(&pool, bodies).await;

    let result = run_turn_bounded(&pool, TurnInFlight::start(conversation.id), Some(hello()), 12, None, false).await;

    let sent = requests.lock().expect("the request log").len();
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    assert_eq!(sent, 4, "the second reasoning-only reply ends the turn: {saved:?}");
    assert_eq!(nudges(&saved), 1, "{saved:?}");
    assert!(result.is_ok(), "the turn ends as a model that meant to stop: {result:?}");
}
