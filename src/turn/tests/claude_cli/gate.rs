//! The five-hour usage gate through a turn's real caller paths (SME-523):
//! a faked reading on the process's gate holds a Claude Code turn's call
//! (G2) and a compaction's summary (G3) until its release, and a Stop
//! during a hold drops the waiter (G4).

use super::*;
use crate::claude_cli::gate::{self, FiveHour, LINE, WINDOW};
use chrono::{Duration, Utc};

/// Clears the process's gate on the way in and out, so a hold never
/// outlives its test.
struct GateReset;

impl GateReset {
    fn new() -> Self {
        gate::reset_for_tests();
        Self
    }
}

impl Drop for GateReset {
    fn drop(&mut self) {
        gate::reset_for_tests();
    }
}

/// A reading that holds the gate for `ms` from now: an hour into the
/// window, at the utilization whose release is then. Read "in the
/// future", so a reading another test streams meanwhile can't replace it.
pub(super) fn hold_for(ms: i64) -> FiveHour {
    let now = Utc::now();
    let resets_at = now + Duration::hours(4);
    let release = now + Duration::milliseconds(ms);
    let share = (release - (resets_at - WINDOW)).num_milliseconds() as f64 / WINDOW.num_milliseconds() as f64;
    FiveHour { utilization: LINE * share, status: "allowed".into(), resets_at: Some(resets_at), seen_at: now + Duration::hours(1) }
}

async fn until_held(conversation_id: i64) {
    for _ in 0..1_000 {
        if gate::gate().waiting().contains(&conversation_id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the call never waited at the gate");
}

/// G2, the Done-when test: a Claude Code turn's call on pace to pass the
/// line waits, with no CLI started, and goes through at the release.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_held_turns_call_waits_for_the_release(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let _gate = GateReset::new();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("gate-turn", vec![vec![json!({"read": true}), text_reply("hi")]]);
    fake.pick_for(&pool, conversation.id).await;
    let started = std::time::Instant::now();
    gate::gate().observe(hold_for(800));

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    until_held(conversation.id).await;
    assert_eq!(fake.started(), 0, "no CLI runs while held");
    tokio::time::timeout(std::time::Duration::from_secs(10), turn).await.expect("the turn ends").expect("join").expect("the turn runs");

    assert!(started.elapsed() >= std::time::Duration::from_millis(800), "it waited for the release: {:?}", started.elapsed());
    assert_eq!(fake.started(), 1);
    assert!(gate::gate().waiting().is_empty());
}

/// G3: a compaction's summary, a Claude Code call of its own, waits too.
/// With only the turn's call gated, the summary's process would start.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_held_summary_waits_for_the_release(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let _gate = GateReset::new();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let full = {
        let mut lines = api_call(31_000, vec![text_block("one")], "end_turn", 3);
        lines.push(crate::claude_cli::tests::fake::result_ok());
        json!({"emit": lines})
    };
    let fake = FakeCli::new("gate-summary", vec![
        vec![json!({"read": true}), json!({"write_session": true}), full],
        vec![json!({"read": true}), text_reply("SUMMARY-GATE")],
        vec![json!({"read": true}), text_reply("two")],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    super::review1::set_cli_window(&pool, provider, 32_768).await;
    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    gate::gate().observe(hold_for(600));

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    until_held(conversation.id).await;
    assert_eq!(fake.started(), 1, "the summary's process waits");
    tokio::time::timeout(std::time::Duration::from_secs(10), turn).await.expect("the turn ends").expect("join").expect("turn 2 compacts and runs");

    assert_eq!(super::compaction::dividers(&pool, conversation.id).await, 1, "it compacted");
    assert_eq!(fake.started(), 3);
}

/// G4: a Stop during a hold ends the turn as stopped and leaves no waiter.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_during_a_hold_drops_the_waiter(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let _gate = GateReset::new();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("gate-stop", vec![vec![json!({"read": true}), text_reply("hi")]]);
    fake.pick_for(&pool, conversation.id).await;
    gate::gate().observe(hold_for(60_000));

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    until_held(conversation.id).await;
    stop_turn_now(conversation.id);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), turn).await.expect("the turn ends").expect("join");

    assert!(ended.expect_err("stopped").to_string().contains(TURN_STOPPED));
    assert!(gate::gate().waiting().is_empty(), "{:?}", gate::gate().waiting());
    assert_eq!(fake.started(), 0);
}

/// `usage::seen` feeds the process's gate as each event streams.
#[test]
fn test_a_streamed_event_reaches_the_gate() {
    let _guard = lock_turn_tests();
    let _gate = GateReset::new();
    let resets = (Utc::now() + Duration::hours(1)).timestamp();
    let event = json!({"type": "rate_limit_event", "rate_limit_info": {"unifiedWindows": {
        "five_hour": {"utilization": 0.99, "status": "allowed", "resetsAt": resets}}}});
    crate::claude_cli::usage::seen(-523_001, &event);
    assert!(matches!(gate::gate().verdict(), gate::Verdict::Held { .. }), "{:?}", gate::gate().verdict());
}
