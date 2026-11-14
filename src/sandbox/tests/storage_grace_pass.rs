//! SME-521: the claim-driven storage-grace pass against the database, the
//! claim list and the removal injected. Each stop the old pass never saw —
//! `terminate_pod`, a finished agent, a lost pod, a failed create, the
//! user's Stop, a pre-change row, claims with no row — and the per-pass
//! cap each have the test the plan names, (a) to (h); (i) is
//! `test_zero_days_touches_nothing`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::storage_grace::{age_pod_end, claims_gone, pass, pass_with, removal_notices, UNBOUNDED};
use super::*;

const SETTINGS: GraceSettings = GraceSettings { default_days: 7, agent_days: 1, user_days: 30 };

/// A conversation whose one pod row was closed by `end`, `days` ago.
async fn ended_days_ago(pool: &PgPool, end: db::PodEnd, days: i32) -> (i64, i64) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(pool, conversation.id).await.expect("pod");
    db::terminate_sandbox_pod_as(pool, pod.id, end).await.expect("end the row");
    age_pod_end(pool, pod.id, days).await;
    (conversation.id, pod.id)
}

/// Makes `conversation_id` a sub-agent in `state`.
async fn make_agent(pool: &PgPool, conversation_id: i64, state: &str) {
    let root = db::create_conversation(pool).await.expect("a root");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'b', $3)")
        .bind(conversation_id)
        .bind(root.id)
        .bind(state)
        .execute(pool)
        .await
        .expect("the agent row");
}

/// A removal that counts its calls and takes both claims.
fn counting(calls: &Arc<AtomicUsize>) -> impl Fn(i64) -> std::future::Ready<Result<StorageRemoval, SandboxError>> {
    let calls = calls.clone();
    move |_conversation| {
        calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(claims_gone()))
    }
}

/// (a) The model's `terminate_pod` (a `Plain` end, no quiet mark) eight
/// days ago: the old pass never saw it; the grace runs from the end.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_terminate_pod_past_the_grace_is_removed(pool: PgPool) {
    let (conversation, pod) = ended_days_ago(&pool, db::PodEnd::Plain, 8).await;
    let settled = pass(&pool, 7, UNBOUNDED, &[conversation], |_conversation| async { Ok(claims_gone()) }).await;
    assert_eq!(settled, vec![conversation]);
    let notices = removal_notices(&pool, conversation).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains(&format!("Sandbox pod {pod}'s storage was removed after 7 days stopped")), "{}", notices[0]);

    let (fresh, _) = ended_days_ago(&pool, db::PodEnd::Plain, 6).await;
    assert!(pass(&pool, 7, UNBOUNDED, &[fresh], |_conversation| async { Ok(claims_gone()) }).await.is_empty(), "6 days of 7");
}

/// (b) A finished sub-agent's storage goes after a day, whatever ended
/// its pod; an active one keeps the default.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_finished_agent_is_removed_after_a_day(pool: PgPool) {
    let (finished, _) = ended_days_ago(&pool, db::PodEnd::Plain, 2).await;
    make_agent(&pool, finished, "finished").await;
    let (active, _) = ended_days_ago(&pool, db::PodEnd::Plain, 2).await;
    make_agent(&pool, active, "active").await;

    let calls = Arc::new(AtomicUsize::new(0));
    let settled = pass(&pool, 7, UNBOUNDED, &[finished, active], counting(&calls)).await;
    assert_eq!(settled, vec![finished], "only the finished agent is past its grace");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let notices = removal_notices(&pool, finished).await;
    assert!(notices.len() == 1 && notices[0].contains("after 1 day finished"), "{notices:?}");
}

/// (c) A lost pod's end, closed the way `close_if_gone` closes it
/// (`force_terminate_pod`'s `Plain`), and (d) a failed create's
/// `terminate_sandbox_pod` on its own fresh row: both removed after the
/// default grace.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_lost_pod_and_a_failed_create_are_removed_after_the_default(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let lost = db::create_conversation(&pool).await.expect("conversation");
    let lost_pod = db::create_sandbox_pod(&pool, lost.id).await.expect("pod");
    terminate_pod_with(&pool, lost_pod.id, async { Ok(()) }).await.expect("the lost pod's close");
    age_pod_end(&pool, lost_pod.id, 8).await;

    let failed = db::create_conversation(&pool).await.expect("conversation");
    let failed_pod = db::create_sandbox_pod(&pool, failed.id).await.expect("pod");
    db::terminate_sandbox_pod(&pool, failed_pod.id).await.expect("the failed create's cleanup");
    age_pod_end(&pool, failed_pod.id, 8).await;

    let settled = pass(&pool, 7, UNBOUNDED, &[lost.id, failed.id], |_conversation| async { Ok(claims_gone()) }).await;
    assert_eq!(settled.len(), 2, "both settled: {settled:?}");
}

/// (e) The user's Stop keeps its storage for its own grace (30 days), and
/// a Stop grace of 0 keeps it for good.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_users_stop_waits_its_own_grace(pool: PgPool) {
    let (eight, _) = ended_days_ago(&pool, db::PodEnd::ByUser, 8).await;
    let (old, _) = ended_days_ago(&pool, db::PodEnd::ByUser, 31).await;
    let settled = pass(&pool, 7, UNBOUNDED, &[eight, old], |_conversation| async { Ok(claims_gone()) }).await;
    assert_eq!(settled, vec![old], "8 days of a Stop's 30 is still grace; 31 is past it");
    assert!(removal_notices(&pool, old).await[0].contains("after 30 days stopped"));

    let (ancient, _) = ended_days_ago(&pool, db::PodEnd::ByUser, 400).await;
    let keep = GraceSettings { user_days: 0, ..SETTINGS };
    assert!(pass_with(&pool, &keep, UNBOUNDED, &[(ancient, 1)], |_conversation| async { Ok(claims_gone()) }).await.is_empty());
}

/// (f) A row ended before the column (flag NULL, no quiet mark), top
/// level: its cause can't be told, so it gets the Stop's grace (Q3).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_from_before_the_change_gets_the_stops_grace(pool: PgPool) {
    let (eight, eight_pod) = ended_days_ago(&pool, db::PodEnd::Plain, 8).await;
    let (old, old_pod) = ended_days_ago(&pool, db::PodEnd::Plain, 31).await;
    sqlx::query("UPDATE sandbox_pods SET stopped_by_user = NULL WHERE id = ANY($1)")
        .bind(vec![eight_pod, old_pod])
        .execute(&pool)
        .await
        .expect("as the upgrade finds them");
    let settled = pass(&pool, 7, UNBOUNDED, &[eight, old], |_conversation| async { Ok(claims_gone()) }).await;
    assert_eq!(settled, vec![old], "kept until 30 days, then removed");
}

/// (g) Claims with no pod row: removed once the oldest claim is past the
/// default, with no notice (no pod ever ran), and not before.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_claims_with_no_row_are_removed_by_their_age_without_a_notice(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let old = db::create_conversation(&pool).await.expect("conversation").id;
    let young = db::create_conversation(&pool).await.expect("conversation").id;
    let calls = Arc::new(AtomicUsize::new(0));
    let settled = pass_with(&pool, &SETTINGS, UNBOUNDED, &[(old, 8), (young, 2)], counting(&calls)).await;
    assert_eq!(settled, vec![old]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(removal_notices(&pool, old).await.is_empty(), "no pod ever ran: nothing to tell");
}

/// (g) The re-check under both locks: a pod started between the list and
/// the re-check makes a newer, live row, and the removal is skipped.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pod_started_after_the_list_skips_the_removal(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation").id;
    let oldest: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT (now() AT TIME ZONE 'UTC') - interval '8 days'").fetch_one(&pool).await.expect("clock");
    let due = Due {
        conversation_id: conversation,
        pod_id: None,
        oldest_claim_utc: Some(oldest),
        at: oldest,
        days: 7,
        class: GraceClass::NoPod,
    };
    assert!(still_due(&pool, &SETTINGS, &due).await, "due as listed");
    db::create_sandbox_pod(&pool, conversation).await.expect("a pod starts");
    assert!(!still_due(&pool, &SETTINGS, &due).await, "a newer row than the one listed: skipped");
}

/// (h) The cap: 25 due, 20 removed in one pass, oldest first; the rest
/// wait for the next pass.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pass_removes_at_most_twenty_oldest_first(pool: PgPool) {
    let mut by_age = Vec::new();
    for days in 8..33 {
        by_age.push(ended_days_ago(&pool, db::PodEnd::Plain, days).await.0);
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let settled = pass(&pool, 7, UNBOUNDED, &by_age, counting(&calls)).await;
    let oldest_twenty: Vec<i64> = by_age.iter().rev().take(STORAGE_REMOVALS_PER_PASS).copied().collect();
    assert_eq!(STORAGE_REMOVALS_PER_PASS, 20);
    assert_eq!(settled, oldest_twenty, "the twenty oldest, oldest first");
    assert_eq!(calls.load(Ordering::SeqCst), 20);

    let settled = pass(&pool, 7, UNBOUNDED, &by_age, counting(&calls)).await;
    assert_eq!(settled.len(), 5, "the next pass takes the rest");
}

/// Review 1, A-L2: the cap counts removals tried. The oldest due
/// conversation busy (its turn running) uses no slot, so a pass over 21
/// due still removes 20, the busy one among none of them.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_busy_conversation_uses_no_slot_of_the_cap(pool: PgPool) {
    let mut by_age = Vec::new();
    for days in 8..29 {
        by_age.push(ended_days_ago(&pool, db::PodEnd::Plain, days).await.0);
    }
    let oldest = *by_age.last().expect("21 due");
    let turn = crate::turn::count_a_test_turn(oldest);
    let calls = Arc::new(AtomicUsize::new(0));
    let settled = pass(&pool, 7, UNBOUNDED, &by_age, counting(&calls)).await;
    drop(turn);
    assert_eq!(settled.len(), STORAGE_REMOVALS_PER_PASS, "twenty removed past the busy one: {settled:?}");
    assert!(!settled.contains(&oldest), "the busy one stands");
    assert_eq!(calls.load(Ordering::SeqCst), 20);
}

/// Review 1, A-L2: a removal tried and left unsettled (a claim left
/// behind) does use its slot, so a stuck one is bounded too.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_removal_uses_its_slot(pool: PgPool) {
    let mut by_age = Vec::new();
    for days in 8..30 {
        by_age.push(ended_days_ago(&pool, db::PodEnd::Plain, days).await.0);
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let settled = pass(&pool, 7, UNBOUNDED, &by_age, move |_conversation| {
        counted.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(StorageRemoval { claims_failed: 1, ..claims_gone() }))
    })
    .await;
    assert!(settled.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), STORAGE_REMOVALS_PER_PASS, "22 due, 20 tried");
}

/// Review 1, A-L3: the pass removes nothing while the conversation's turn
/// runs, its turn lock is held, or its start lock is; once free, it does.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_pass_skips_while_a_turn_runs_or_a_lock_is_held(pool: PgPool) {
    let (conversation, _) = ended_days_ago(&pool, db::PodEnd::Plain, 8).await;
    let calls = Arc::new(AtomicUsize::new(0));

    let turn = crate::turn::count_a_test_turn(conversation);
    assert!(pass(&pool, 7, UNBOUNDED, &[conversation], counting(&calls)).await.is_empty(), "a turn running");
    drop(turn);

    let lock = crate::turn::conversation_lock(conversation);
    let held = lock.lock().await;
    assert!(pass(&pool, 7, UNBOUNDED, &[conversation], counting(&calls)).await.is_empty(), "the turn lock held");
    drop(held);
    drop(lock);

    let start = pod_start_lock(conversation);
    let starting = start.lock().await;
    assert!(pass(&pool, 7, UNBOUNDED, &[conversation], counting(&calls)).await.is_empty(), "the start lock held");
    drop(starting);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing removed while busy");

    assert_eq!(pass(&pool, 7, UNBOUNDED, &[conversation], counting(&calls)).await, vec![conversation], "removed once free");
}

/// Review 1, A-L3: an agent woken (`finished` → `active`) between the
/// list and the re-check isn't due any more: 2 days after its end is past
/// the agent's day but within the default week.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_agent_woken_after_the_list_skips_the_removal(pool: PgPool) {
    let (conversation, pod) = ended_days_ago(&pool, db::PodEnd::Plain, 2).await;
    make_agent(&pool, conversation, "finished").await;
    let at: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT terminated_at + interval '1 day' FROM sandbox_pods WHERE id = $1")
            .bind(pod)
            .fetch_one(&pool)
            .await
            .expect("the agent's due time");
    let due = Due { conversation_id: conversation, pod_id: Some(pod), oldest_claim_utc: None, at, days: 1, class: GraceClass::Agent };
    assert!(still_due(&pool, &SETTINGS, &due).await, "due as listed");
    sqlx::query("UPDATE agents SET state = 'active' WHERE conversation_id = $1")
        .bind(conversation)
        .execute(&pool)
        .await
        .expect("the parent's message wakes it");
    assert!(!still_due(&pool, &SETTINGS, &due).await, "woken: skipped");
}

/// Review 1, A-L3: a conversation whose latest row is live is never
/// removed through the pass, however old its claims or done its agent.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_live_row_is_never_removed_through_the_pass(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, _) = ended_days_ago(&pool, db::PodEnd::Plain, 40).await;
    db::create_sandbox_pod(&pool, conversation).await.expect("a live pod after the old one");
    make_agent(&pool, conversation, "finished").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let settled = pass_with(&pool, &SETTINGS, UNBOUNDED, &[(conversation, 400)], counting(&calls)).await;
    assert!(settled.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0, "never tried");
}
