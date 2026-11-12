//! A persona's token ceiling on a Claude Code provider (SME-493): with a
//! 1,000,000 window far above the usage, only the ceiling can compact, and
//! its compaction restarts the CLI session from the summary as every other
//! compaction does (SME-431).

use super::compaction::{assert_summary_call, live_ids};
use super::*;

/// C1: two tool steps in one CLI run, the second at 50,000 (over the
/// 40,000 ceiling): the turn compacts before its third call. The summary
/// is a one-shot process, the parked run is interrupted, and the next call
/// is a new session whose first message holds the summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claude_code_seat_compacts_at_its_ceiling_and_restarts_its_session(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let id = conversation.id;
    let tool = |call: &str| crate::claude_cli::tests::fake::tool_block(call, "todoread", &json!({}));
    let bridged = |call: &str| json!({"call": [{"id": call, "name": "mcp__smelt__todoread", "input": {}}]});
    let fake = FakeCli::new("sme493-ceiling", vec![
        vec![
            json!({"read": true}),
            json!({"write_session": true}),
            json!({"emit": api_call(30_000, vec![tool("toolu_c1")], "tool_use", 5)}),
            bridged("toolu_c1"),
            json!({"emit": api_call(50_000, vec![tool("toolu_c2")], "tool_use", 5)}),
            bridged("toolu_c2"),
        ],
        vec![json!({"read": true}), text_reply("SUMMARY-C1")],
        vec![json!({"read": true}), text_reply("after the summary")],
    ]);
    let provider = fake.pick_for(&pool, id).await;
    super::review1::set_cli_window(&pool, provider, 1_000_000).await;
    super::super::compact_at::set_persona_ceiling(&pool, id, 40_000).await;
    let live = live_ids(&pool, id).await;

    run_turn(&pool, id, hello()).await.expect("the turn compacts and carries on");

    assert_eq!(fake.started(), 3, "the run, the summary, the new session");
    assert_summary_call(&fake, 1, &live);
    assert!(fake.stdin(0).iter().any(is_interrupt), "the parked run was interrupted");
    let old = {
        let args = fake.args(0);
        let at = args.iter().position(|a| a == "--session-id").expect("process 0 starts a session");
        args[at + 1].clone()
    };
    assert!(fake.args(2).iter().any(|a| a == "--session-id"), "a new session: {:?}", fake.args(2));
    assert!(!fake.args(2).contains(&old), "not the old session: {:?}", fake.args(2));
    assert!(user_texts(&fake, 2)[0].contains("SUMMARY-C1"), "the new session starts from the summary: {:?}", user_texts(&fake, 2));
    let divided = super::super::on_request::dividers(&pool, id).await;
    assert_eq!(divided, vec![("SUMMARY-C1".to_string(), Some("it passed its persona's 40,000-token ceiling".to_string()))]);
    let session = db::get_claude_cli_session(&pool, id).await.expect("read").expect("the new session's row");
    assert!(fake.args(2).contains(&session.session_id), "the row is the new session's");
}
