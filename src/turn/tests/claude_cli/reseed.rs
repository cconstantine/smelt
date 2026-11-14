//! Re-seeding a Claude Code conversation's prompt cache (SME-523): each of
//! the watch's conditions holding a re-seed back (R2), a re-seed through
//! the fake CLI (R3), and a re-seed finishing after a real call (R4).

use super::*;
use crate::claude_cli::reseed::{self as candidates, Candidate};
use crate::turn::reseed::{CACHE_TTL, Decision, MARGIN, RESEED_AFTER, RESEED_CAP, consider, run_tick};
use chrono::{DateTime, Duration, Utc};

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

/// A conversation every condition lets through: on a fake Claude Code
/// provider, a session recorded for it, a command running, and a
/// candidate whose call started `RESEED_AFTER` before `now`.
struct Ready {
    conversation: i64,
    candidate: Candidate,
    fake: FakeCli,
    now: DateTime<Utc>,
}

const SESSION: &str = "00000000-0000-4000-8000-000000000523";

async fn ready(pool: &PgPool, name: &str, runs: Vec<Vec<Value>>) -> Ready {
    let conversation = db::create_conversation(pool).await.expect("conversation").id;
    let fake = FakeCli::new(name, runs);
    let provider = fake.pick_for(pool, conversation).await;
    let session = db::ClaudeCliSession { provider_id: provider, session_id: SESSION.to_string(), synced_count: 0, synced_hash: String::new() };
    db::set_claude_cli_session(pool, conversation, &session).await.expect("a session");
    let pod = db::create_sandbox_pod(pool, conversation).await.expect("a pod");
    let terminal = db::create_sandbox_terminal(pool, pod.id).await.expect("a terminal");
    db::create_terminal_command(pool, conversation, terminal.id, &format!("reseed-{conversation}"), "sleep 600", 300).await.expect("a running command");
    let now = Utc::now();
    candidates::note_call(conversation, provider, now - RESEED_AFTER);
    let candidate = candidates::get(conversation).expect("a candidate");
    Ready { conversation, candidate, fake, now }
}

fn assert_held_back(decision: Decision, fake: &FakeCli, what: &str) {
    assert!(!matches!(decision, Decision::Launch(_)), "{what}: re-seeded anyway");
    assert_eq!(fake.started(), 0, "{what}: a process started");
}

/// The setup itself re-seeds: each test below takes one condition away.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_due_candidate_with_every_condition_is_re_seeded(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-all", vec![]).await;
    assert!(matches!(consider(&pool, r.conversation, r.candidate, r.now).await, Decision::Launch(_)));
}

/// R2: a running turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_while_a_turn_runs(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-turn", vec![]).await;
    let _turn = crate::turn::count_a_test_turn(r.conversation);
    let decision = consider(&pool, r.conversation, r.candidate, r.now).await;
    assert!(matches!(decision, Decision::Wait(_)), "the next tick looks again");
    assert_held_back(decision, &r.fake, "a running turn");
}

/// R2: a cache already past its TTL, less the margin.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_of_a_dead_cache(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-dead", vec![]).await;
    let candidate = Candidate { start: r.now - CACHE_TTL + MARGIN, ..r.candidate };
    let decision = consider(&pool, r.conversation, candidate, r.now).await;
    assert!(matches!(decision, Decision::Drop(_)));
    assert_held_back(decision, &r.fake, "a dead cache");
}

/// R2: nothing expected to wake it (its command finished).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_without_a_wake_source(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-nowake", vec![]).await;
    db::mark_terminal_command_finished(&pool, &format!("reseed-{}", r.conversation), 0).await.expect("finish it");
    let decision = consider(&pool, r.conversation, r.candidate, r.now).await;
    assert!(matches!(decision, Decision::Drop(_)));
    assert_held_back(decision, &r.fake, "no wake source");
}

/// R2: the cap reached this idle spell.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_past_the_cap(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-cap", vec![]).await;
    let candidate = Candidate { reseeds: RESEED_CAP, ..r.candidate };
    let decision = consider(&pool, r.conversation, candidate, r.now).await;
    assert!(matches!(decision, Decision::Drop(_)));
    assert_held_back(decision, &r.fake, "the cap");
}

/// R2: the conversation's model moved to another provider since its call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_on_another_provider(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-provider", vec![]).await;
    let candidate = Candidate { provider_id: r.candidate.provider_id + 1_000_000, ..r.candidate };
    let decision = consider(&pool, r.conversation, candidate, r.now).await;
    assert!(matches!(decision, Decision::Drop(_)));
    assert_held_back(decision, &r.fake, "another provider");
}

/// R2: the usage gate held. A re-seed never queues.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_while_the_gate_is_held(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-gate", vec![]).await;
    crate::claude_cli::gate::gate().observe(super::gate::hold_for(60_000));
    let decision = consider(&pool, r.conversation, r.candidate, r.now).await;
    assert!(matches!(decision, Decision::Wait(_)));
    assert_held_back(decision, &r.fake, "a held gate");
    assert!(crate::claude_cli::gate::gate().waiting().is_empty(), "it didn't queue");
}

/// R2: the provider's request limit has no slot free.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_re_seed_without_a_free_slot(pool: PgPool) {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let r = ready(&pool, "reseed-slot", vec![]).await;
    let provider = r.candidate.provider_id;
    crate::providers::limiter::set(provider, Utc::now().naive_utc(), Some(1));
    let _held = crate::providers::limiter::current(provider).expect("a pool").try_acquire().expect("its one slot");
    let decision = consider(&pool, r.conversation, r.candidate, r.now).await;
    crate::providers::limiter::forget(provider);
    assert!(matches!(decision, Decision::Wait(_)));
    assert_held_back(decision, &r.fake, "a busy slot");
}

/// R3: a due candidate is re-seeded through the CLI: its session resumed
/// unsaved, one turn, the turn's tools, the one line; nothing saved to the
/// conversation, a `reseed` call recorded, and its reading at the gate.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_re_seed_resumes_the_session_unsaved_and_is_recorded(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let resets = (Utc::now() + Duration::hours(1)).timestamp();
    let rate = json!({"type": "rate_limit_event", "rate_limit_info": {"unifiedWindows": {
        "five_hour": {"utilization": 0.99, "status": "allowed", "resetsAt": resets}}}});
    let mut reply = api_call(10, vec![text_block("ok")], "end_turn", 1);
    reply[0]["event"]["message"]["usage"]["cache_read_input_tokens"] = json!(40_000);
    let mut lines = vec![rate];
    lines.extend(reply);
    lines.push(crate::claude_cli::tests::fake::result_ok());
    let r = ready(&pool, "reseed-run", vec![vec![json!({"read": true}), json!({"list_tools": true}), json!({"emit": lines})]]).await;

    let started = run_tick(&pool, Utc::now()).await;
    assert_eq!(started.len(), 1, "one re-seed");
    for handle in started {
        tokio::time::timeout(std::time::Duration::from_secs(20), handle).await.expect("it ends").expect("join");
    }

    assert_eq!(r.fake.started(), 1);
    let args = r.fake.args(0);
    let resumed = args.iter().position(|a| a == "--resume").and_then(|i| args.get(i + 1));
    assert_eq!(resumed.map(String::as_str), Some(SESSION), "{args:?}");
    let turns = args.iter().position(|a| a == "--max-turns").and_then(|i| args.get(i + 1));
    assert_eq!(turns.map(String::as_str), Some("1"), "{args:?}");
    assert!(args.iter().any(|a| a == "--no-session-persistence"), "{args:?}");
    let config = args.iter().position(|a| a == "--mcp-config").and_then(|i| args.get(i + 1)).expect("the turn's tools");
    assert!(config.ends_with(".json"), "{config}");
    // Review 1 L4: the config's server serves the turn's own tools.
    let turn_model = crate::providers::resolve_turn_model(&pool, r.conversation).await.expect("its model");
    let (turn_request, _) = crate::turn::reply::build_request(&pool, r.conversation, &turn_model, None, &[], false, 0).await.expect("a turn's request");
    let prefix = format!("mcp__{}__", crate::claude_cli::bridge::SERVER_NAME);
    let mut expected: Vec<String> =
        turn_request.tools.iter().map(|t| crate::claude_cli::bridge::cli_tool_name(&t.name).trim_start_matches(&prefix).to_string()).collect();
    let mut served = r.fake.tools(0);
    expected.sort();
    served.sort();
    assert!(!expected.is_empty(), "a turn has tools");
    assert_eq!(served, expected, "the re-seed's tools are the turn's");
    assert_eq!(user_texts(&r.fake, 0), vec![candidates::MESSAGE.to_string()]);
    assert!(saved(&pool, r.conversation).await.is_empty(), "the reply isn't saved");
    let kinds: Vec<(String, i64)> = sqlx::query_as("SELECT kind, cache_read_input_tokens FROM model_call_usage WHERE conversation_id = $1")
        .bind(r.conversation)
        .fetch_all(&pool)
        .await
        .expect("the calls");
    assert_eq!(kinds, vec![("reseed".to_string(), 40_000)]);
    assert!(matches!(crate::claude_cli::gate::gate().verdict(), crate::claude_cli::gate::Verdict::Held { .. }), "its reading reached the gate");
    let after = candidates::get(r.conversation).expect("still a candidate");
    assert_eq!(after.reseeds, 1);
    assert!(after.start > r.candidate.start && !after.in_flight, "{after:?}");
}

/// R4: a re-seed that ends after a real call doesn't move its start back,
/// nor count against the new spell; one with no call since does both.
#[test]
fn test_a_re_seed_ending_after_a_real_call_keeps_the_calls_start() {
    let _guard = lock_turn_tests();
    let _clean = Clean::new();
    let t0 = Utc::now() - Duration::minutes(60);
    let (reseed_start, call_start) = (t0 + Duration::minutes(50), t0 + Duration::minutes(55));
    candidates::note_call(-523_004, 7, t0);
    assert!(candidates::begin(-523_004));
    assert!(!candidates::begin(-523_004), "one at a time");
    candidates::note_call(-523_004, 7, call_start);
    candidates::note_reseed(-523_004, reseed_start);
    let kept = candidates::get(-523_004).expect("a candidate");
    assert_eq!((kept.start, kept.reseeds), (call_start, 0));

    candidates::note_call(-523_005, 7, t0);
    assert!(candidates::begin(-523_005));
    candidates::note_reseed(-523_005, reseed_start);
    let moved = candidates::get(-523_005).expect("a candidate");
    assert_eq!((moved.start, moved.reseeds, moved.in_flight), (reseed_start, 1, false));
}
