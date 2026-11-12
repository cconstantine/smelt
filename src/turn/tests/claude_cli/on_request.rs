//! Compacting on demand on a Claude Code provider (SME-482): the endpoint
//! and the model's `compact_context` reset the CLI session as every other
//! compaction does (SME-431), so the next call starts a new session seeded
//! from the summary. T14 and T15 of the plan.

use super::compaction::{assert_summary_call, dividers, live_ids};
use super::*;

/// T14: the endpoint compacts between turns: the summary is a one-shot
/// call, the session row goes, and the next turn is a new session seeded
/// from the summary, its old session file gone.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_endpoint_resets_a_claude_code_session(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let id = conversation.id;
    let fake = FakeCli::new("sme482-endpoint", vec![
        vec![json!({"read": true}), json!({"write_session": true}), text_reply("one")],
        vec![json!({"read": true}), text_reply("SUMMARY-T14")],
        vec![json!({"read": true}), text_reply("two")],
    ]);
    fake.pick_for(&pool, id).await;
    run_turn(&pool, id, hello()).await.expect("turn 1");
    let old = db::get_claude_cli_session(&pool, id).await.expect("read").expect("turn 1's session").session_id;
    assert!(super::review3::session_file(id, &old).exists(), "turn 1's session file");
    let live = live_ids(&pool, id).await;

    crate::turn::compact_now(&pool, id, None).await.expect("compacted");

    assert_eq!(fake.started(), 2, "turn 1 and the summary");
    assert_summary_call(&fake, 1, &live);
    assert_eq!(dividers(&pool, id).await, 1);
    assert!(db::get_claude_cli_session(&pool, id).await.expect("read").is_none(), "the session row went");
    run_turn(&pool, id, hello()).await.expect("turn 2");
    assert_eq!(fake.started(), 3, "turn 2 is a process of its own");
    assert!(fake.args(2).iter().any(|a| a == "--session-id"), "a new session: {:?}", fake.args(2));
    assert!(!fake.args(2).contains(&old), "not turn 1's session: {:?}", fake.args(2));
    assert!(user_texts(&fake, 2)[0].contains("SUMMARY-T14"), "{:?}", user_texts(&fake, 2));
    assert!(!super::review3::session_file(id, &old).exists(), "turn 1's session file went");
}

/// A reply calling each of `calls` (id, name) as the CLI streams it, and
/// the bridge calls it makes for them.
fn calls_reply(calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let blocks = calls.iter().map(|(id, name, input)| crate::claude_cli::tests::fake::tool_block(id, name, input)).collect();
    let bridged: Vec<Value> =
        calls.iter().map(|(id, name, input)| json!({"id": id, "name": format!("mcp__smelt__{name}"), "input": input})).collect();
    vec![
        json!({"read": true}),
        json!({"write_session": true}),
        json!({"emit": api_call(100, blocks, "tool_use", 5)}),
        json!({"call": bridged}),
    ]
}

/// The session process `n` started, from its `--session-id`.
fn session_of(fake: &FakeCli, n: usize) -> String {
    let args = fake.args(n);
    let at = args.iter().position(|a| a == "--session-id").unwrap_or_else(|| panic!("process {n} starts a session: {args:?}"));
    args[at + 1].clone()
}

/// Process `n` is a session of its own, not a resume of `old`.
fn assert_new_session(fake: &FakeCli, n: usize, old: &str) {
    let args = fake.args(n);
    assert!(!args.iter().any(|a| a == "--resume"), "not a resume: {args:?}");
    assert_ne!(session_of(fake, n), old, "not the old session: {args:?}");
}

/// T15, mid-turn: the model's compact_context compacts at the end of its
/// pass; the parked run is interrupted and the turn's next call is a new
/// session seeded from the summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_models_compaction_starts_a_new_claude_code_session_mid_turn(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("sme482-tool", vec![
        calls_reply(&[("toolu_c", "compact_context", json!({}))]),
        vec![json!({"read": true}), text_reply("SUMMARY-T15")],
        vec![json!({"read": true}), text_reply("after")],
    ]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn compacts and carries on");

    assert_eq!(fake.started(), 3, "the run, the summary, the new session");
    assert!(fake.stdin(0).iter().any(is_interrupt), "the parked run was interrupted");
    assert_new_session(&fake, 2, &session_of(&fake, 0));
    assert!(user_texts(&fake, 2)[0].contains("SUMMARY-T15"), "{:?}", user_texts(&fake, 2));
    assert_eq!(dividers(&pool, conversation.id).await, 1);
    let saved = saved(&pool, conversation.id).await;
    assert!(matches!(&saved.last().expect("a reply").1[..], [anthropic::ContentBlock::Text { text }] if text == "after"), "{saved:?}");
}

/// T15, ending the turn: compact_context beside notify_user compacts
/// before the turn returns, and the next turn starts a new session from
/// the summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_models_compaction_at_a_turns_end_resets_the_next_turns_session(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("sme482-tool-end", vec![
        calls_reply(&[("toolu_c", "compact_context", json!({})), ("toolu_n", "notify_user", json!({"message": "done"}))]),
        vec![json!({"read": true}), text_reply("SUMMARY-T15-END")],
        vec![json!({"read": true}), text_reply("next")],
    ]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn compacts and ends");
    assert_eq!(fake.started(), 2, "the run and the summary");
    assert_eq!(dividers(&pool, conversation.id).await, 1);
    // The reset itself, before any call could find the history's hash
    // changed (the other guard, `render::tail_after`): the row is gone.
    let row = db::get_claude_cli_session(&pool, conversation.id).await.expect("read");
    assert!(row.is_none(), "the compaction ended the session: {row:?}");

    run_turn(&pool, conversation.id, hello()).await.expect("the next turn");
    assert_eq!(fake.started(), 3);
    assert_new_session(&fake, 2, &session_of(&fake, 0));
    assert!(user_texts(&fake, 2)[0].contains("SUMMARY-T15-END"), "{:?}", user_texts(&fake, 2));
}
