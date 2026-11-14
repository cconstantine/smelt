//! Re-seeding's guards from PR #353's review 1: none for a conversation
//! paused by a Stop (M3); a re-seed that writes its session undone, and
//! re-seeding off after (M2); a re-seed and a turn never at once on a
//! conversation (M2).

use super::*;
use crate::claude_cli::reseed as candidates;
use crate::turn::reseed::{Decision, RESEED_AFTER, consider, run_tick};
use chrono::Utc;

/// A re-seed's reply: one API call answering `ok`, and its result.
fn reply() -> Vec<Value> {
    let mut lines = api_call(10, vec![text_block("ok")], "end_turn", 1);
    lines.push(crate::claude_cli::tests::fake::result_ok());
    lines
}

/// Runs one tick and waits for the re-seeds it started; returns how many.
async fn tick_and_wait(pool: &PgPool) -> usize {
    let started = run_tick(pool, Utc::now()).await;
    let count = started.len();
    for handle in started {
        tokio::time::timeout(std::time::Duration::from_secs(20), handle).await.expect("it ends").expect("join");
    }
    count
}

/// Clears the candidates and the gate on the way in and out.
struct Clean;

impl Clean {
    fn new() -> Self {
        candidates::reset_for_tests();
        crate::claude_cli::gate::reset_for_tests();
        Self
    }
}

impl Drop for Clean {
    fn drop(&mut self) {
        candidates::reset_for_tests();
        crate::claude_cli::gate::reset_for_tests();
    }
}

const SESSION: &str = "00000000-0000-4000-8000-000000000524";

/// A conversation every condition lets through (as `reseed.rs`'s
/// `ready`): its id, its candidate's provider and the fake CLI.
async fn ready(pool: &PgPool, name: &str, runs: Vec<Vec<Value>>) -> (i64, i64, FakeCli) {
    let conversation = db::create_conversation(pool).await.expect("conversation").id;
    let fake = FakeCli::new(name, runs);
    let provider = fake.pick_for(pool, conversation).await;
    let session = db::ClaudeCliSession { provider_id: provider, session_id: SESSION.to_string(), synced_count: 0, synced_hash: String::new() };
    db::set_claude_cli_session(pool, conversation, &session).await.expect("a session");
    let pod = db::create_sandbox_pod(pool, conversation).await.expect("a pod");
    let terminal = db::create_sandbox_terminal(pool, pod.id).await.expect("a terminal");
    db::create_terminal_command(pool, conversation, terminal.id, &format!("reseed-{conversation}"), "sleep 600", 300).await.expect("a running command");
    candidates::note_call(conversation, provider, Utc::now() - RESEED_AFTER);
    (conversation, provider, fake)
}

/// R2, review 1 M3: a conversation the user stopped isn't re-seeded: a
/// Stop pauses it until the user writes, so no wake will read the cache.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_of_a_stopped_conversation(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let (conversation, _, fake) = ready(&pool, "reseed-paused", vec![]).await;
    crate::turn::stop_turn_now(conversation);
    let candidate = candidates::get(conversation).expect("a candidate");
    let decision = consider(&pool, conversation, candidate, Utc::now()).await;
    crate::turn::resume_turns(conversation);
    assert!(matches!(decision, Decision::Drop(_)), "a paused conversation is dropped");
    assert_eq!(fake.started(), 0);
}

/// Review 1 M2: a re-seed whose CLI wrote to the session despite
/// `--no-session-persistence` has the file put back as it was, and no
/// conversation is re-seeded after.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_re_seed_that_writes_its_session_is_undone_and_turns_re_seeding_off(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let mut run = vec![json!({"read": true}), json!({"write_session": "append"})];
    run.push(json!({"emit": reply()}));
    let (conversation, _, fake) = ready(&pool, "reseed-writes", vec![run]).await;
    let file = super::review3::session_file(conversation, SESSION);
    std::fs::create_dir_all(file.parent().expect("its dir")).expect("the sessions dir");
    let before = "{\"before\":true}\n";
    std::fs::write(&file, before).expect("the session file");

    assert_eq!(tick_and_wait(&pool).await, 1, "one re-seed");
    assert_eq!(fake.started(), 1);
    assert_eq!(std::fs::read_to_string(&file).expect("the session file"), before, "put back as it was");
    assert!(candidates::off(), "re-seeding is off");

    let (other, _, other_fake) = ready(&pool, "reseed-after-off", vec![]).await;
    let candidate = candidates::get(other).expect("a candidate");
    assert!(matches!(consider(&pool, other, candidate, Utc::now()).await, Decision::Drop(_)), "no more re-seeds");
    assert_eq!(other_fake.started(), 0);
}

/// Review 1 M2: a re-seed holds its conversation's turn lock while it
/// runs, so a turn waits for it; and none starts while a turn holds it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_re_seed_and_a_turn_never_run_at_once(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let mut run = vec![json!({"read": true}), json!({"sleep_ms": 1_000})];
    run.push(json!({"emit": reply()}));
    let (conversation, _, fake) = ready(&pool, "reseed-lock", vec![run]).await;
    let lock = crate::turn::conversation_lock(conversation);

    let held = lock.clone().try_lock_owned().expect("free");
    let candidate = candidates::get(conversation).expect("a candidate");
    let decision = consider(&pool, conversation, candidate, Utc::now()).await;
    assert!(matches!(decision, Decision::Wait(_)), "the next tick looks again");
    assert!(crate::claude_cli::gate::try_pass(conversation), "it didn't take the gate's pass");
    drop(held);

    let started = run_tick(&pool, Utc::now()).await;
    assert_eq!(started.len(), 1, "one re-seed");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(lock.try_lock().is_err(), "the re-seed holds the turn lock");
    for handle in started {
        tokio::time::timeout(std::time::Duration::from_secs(20), handle).await.expect("it ends").expect("join");
    }
    assert!(lock.try_lock().is_ok(), "and lets it go");
    assert_eq!(fake.started(), 1);
}
