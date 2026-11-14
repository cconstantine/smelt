//! Re-seeding's guards from PR #353's review 1: none for a conversation
//! paused by a Stop (M3).

use super::*;
use crate::claude_cli::reseed as candidates;
use crate::turn::reseed::{Decision, RESEED_AFTER, consider};
use chrono::Utc;

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
