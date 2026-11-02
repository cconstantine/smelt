//! SME-345: the storage-grace pass against the database, the removal
//! injected — the settle rules (retry, suppress, one-shot clear) each
//! have the test they name, none merely the pass.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::*;

/// The settle tests don't care about the per-candidate bound (F1); they
/// give the removal an hour.
const UNBOUNDED: Duration = Duration::from_secs(3600);

/// A conversation whose pod row was quiet-stopped and aged `days` back.
async fn quiet_stopped_days_ago(pool: &PgPool, days: i32) -> (i64, i64) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(pool, conversation.id).await.expect("pod");
    sqlx::query("UPDATE sandbox_pods SET terminated_at = now(), quietly_stopped_at = now() - make_interval(days => $2) WHERE id = $1")
        .bind(pod.id)
        .bind(days)
        .execute(pool)
        .await
        .expect("quiet-stop the row");
    (conversation.id, pod.id)
}

async fn mark_set(pool: &PgPool, pod_id: i64) -> bool {
    sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>("SELECT quietly_stopped_at FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_one(pool)
        .await
        .expect("read the mark")
        .is_some()
}

/// The storage-removal notices in `conversation_id`'s transcript.
async fn removal_notices(pool: &PgPool, conversation_id: i64) -> Vec<String> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("list messages")
        .into_iter()
        .filter(|m| m.content.contains("storage was removed"))
        .map(|m| m.content)
        .collect()
}

/// Both claims deleted, the pass's usual outcome.
fn claims_gone() -> StorageRemoval {
    StorageRemoval { claims_deleted: 2, ..Default::default() }
}

#[test]
fn test_storage_removal_days_is_a_week_by_default_and_zero_turns_it_off() {
    assert_eq!(storage_removal_days_from(None), 7);
    assert_eq!(storage_removal_days_from(Some(String::new())), 7, "an empty setting is no setting");
    assert_eq!(storage_removal_days_from(Some("0".to_string())), 0, "0 is off, not a fallback");
    assert_eq!(storage_removal_days_from(Some("30".to_string())), 30);
    assert_eq!(storage_removal_days_from(Some("365".to_string())), 365, "a year is the ceiling");
    // 604800 is a week written in seconds: the ceiling catches it and the
    // default answers — not ~1,657 years of grace.
    for bad in ["366", "604800", "-1", "junk", "2.5", " 7 "] {
        assert_eq!(storage_removal_days_from(Some(bad.to_string())), 7, "{bad:?} falls back with a warning");
    }
}

/// The pair the plan names: due → one removal, one notice, mark cleared,
/// and the next pass says nothing. Red without the pass; red without the
/// settle each following test names.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_due_quiet_stop_is_removed_once_noticed_and_settled(pool: PgPool) {
    let (conversation, pod) = quiet_stopped_days_ago(&pool, 8).await;
    let calls = Arc::new(AtomicUsize::new(0));

    let remove = {
        let calls = calls.clone();
        move |_conversation: i64| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(claims_gone()) }
        }
    };
    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, remove.clone()).await;

    assert_eq!(settled, vec![conversation], "the due conversation settled");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one removal");
    assert!(!mark_set(&pool, pod).await, "the cleared mark makes the stop one-shot");
    let notices = removal_notices(&pool, conversation).await;
    assert_eq!(notices.len(), 1, "one notice: {notices:?}");
    assert!(notices[0].contains(&format!("Sandbox pod {pod}'s storage was removed")), "{}", notices[0]);
    assert!(notices[0].contains("removed after 7 days stopped"), "{}", notices[0]);
    assert!(notices[0].contains("starts fresh with empty volumes"), "{}", notices[0]);

    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, remove).await;
    assert_eq!(settled, Vec::<i64>::new(), "the second pass has nothing to do");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "and never removes again");
    assert_eq!(removal_notices(&pool, conversation).await.len(), 1, "and says it once");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_within_the_grace_is_never_touched(pool: PgPool) {
    let (conversation, pod) = quiet_stopped_days_ago(&pool, 1).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();

    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, move |_conversation| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Ok(claims_gone()) }
    })
    .await;

    assert_eq!(settled, Vec::<i64>::new(), "day one of a week's grace is not due");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the removal is never called");
    assert!(mark_set(&pool, pod).await, "the clock runs on");
    assert!(removal_notices(&pool, conversation).await.is_empty());
}

/// The retry rule: an error keeps the mark and says nothing; the next
/// pass starts from the top and settles, and only now the notice.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failing_removal_keeps_the_mark_and_retries_next_pass(pool: PgPool) {
    let (conversation, pod) = quiet_stopped_days_ago(&pool, 8).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();

    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, move |_conversation: i64| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Err(SandboxError::StartFailed("the API server is down".into())) }
    })
    .await;

    assert_eq!(settled, Vec::<i64>::new(), "the error settles nothing");
    assert!(mark_set(&pool, pod).await, "the mark is kept for the retry");
    assert!(removal_notices(&pool, conversation).await.is_empty(), "nothing was removed, nothing is claimed");

    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, {
        let calls = calls.clone();
        move |_conversation: i64| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(claims_gone()) }
        }
    })
    .await;
    assert_eq!(settled, vec![conversation], "the next pass retries from the top");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(removal_notices(&pool, conversation).await.len(), 1, "and notices once, for the stop");
}

/// `claims_failed` is a half-removal: the disk may still hold the
/// workspace, so no notice sends the model off to re-clone, and the
/// mark keeps the next pass finishing the job.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claim_left_behind_retries_and_holds_its_notice(pool: PgPool) {
    let (conversation, pod) = quiet_stopped_days_ago(&pool, 8).await;
    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, |_conversation| async {
        Ok(StorageRemoval { claims_deleted: 1, claims_failed: 1, ..Default::default() })
    })
    .await;

    assert_eq!(settled, Vec::<i64>::new(), "a half-removal is not settled");
    assert!(mark_set(&pool, pod).await, "the mark is kept: the next pass finishes it");
    assert!(removal_notices(&pool, conversation).await.is_empty(), "and no notice while files may still stand");
}

/// Claims already gone — the user removed the storage after the stop, or
/// a `Foreign` claim no retry could take — settles quietly, once: a
/// notice here would report a removal that never happened.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_claims_already_gone_settle_quietly_and_once(pool: PgPool) {
    let (conversation, pod) = quiet_stopped_days_ago(&pool, 8).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();

    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, move |_conversation: i64| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Ok(StorageRemoval::default()) }
    })
    .await;

    assert_eq!(settled, vec![conversation], "nothing to delete is still a settled stop");
    assert!(!mark_set(&pool, pod).await, "the mark clears — retrying could never help");
    assert!(removal_notices(&pool, conversation).await.is_empty(), "and no notice for a removal that didn't happen");

    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, {
        let calls = calls.clone();
        move |_conversation| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(StorageRemoval::default()) }
        }
    })
    .await;
    assert_eq!(settled, Vec::<i64>::new(), "and it is never asked again");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// The race the pass can actually meet: a start queued behind the locks
/// at candidate time, stopped by the removal itself. `remove_conversation_storage_locked`
/// saved its own notice for it, so the pass suppresses its own — one
/// notice, no surprise pair.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_removal_that_stopped_a_pod_saved_its_own_notice(pool: PgPool) {
    let (conversation, pod) = quiet_stopped_days_ago(&pool, 8).await;
    let settled = remove_expired_storage_once_with(&pool, 7, UNBOUNDED, |_conversation| async {
        Ok(StorageRemoval { pod_stopped: true, claims_deleted: 2, ..Default::default() })
    })
    .await;

    assert_eq!(settled, vec![conversation]);
    assert!(!mark_set(&pool, pod).await, "settled all the same");
    assert!(removal_notices(&pool, conversation).await.is_empty(), "the removal's notice is the only one the pass leaves");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_zero_days_touches_nothing(pool: PgPool) {
    let (conversation, pod) = quiet_stopped_days_ago(&pool, 99).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();

    let settled = remove_expired_storage_once_with(&pool, 0, UNBOUNDED, move |_conversation| {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Ok(claims_gone()) }
    })
    .await;

    assert_eq!(settled, Vec::<i64>::new(), "off means off, however ancient the stop");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(mark_set(&pool, pod).await);
    assert!(removal_notices(&pool, conversation).await.is_empty());
}

/// The bound F1 (SME-345 review 1) adds: the removal runs holding the
/// conversation's turn lock, so it may not run unbounded. A removal that
/// doesn't finish in time settles nothing — mark kept, nothing claimed —
/// and the pass goes on to the next candidate rather than freezing on it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_removal_given_up_on_keeps_its_mark_and_the_pass_goes_on(pool: PgPool) {
    // The stalled one first: older marks sort first, so it is the pass's
    // first candidate and the hour it sleeps for is the stall.
    let (stalled, stalled_pod) = quiet_stopped_days_ago(&pool, 9).await;
    let (on_time, on_time_pod) = quiet_stopped_days_ago(&pool, 8).await;

    let settled = remove_expired_storage_once_with(&pool, 7, Duration::from_millis(50), move |conversation| async move {
        if conversation == stalled {
            // Ten times the pass's patience here (production's is the
            // 90 s constant): a bound removal is cancelled in its sleep.
            tokio::time::sleep(Duration::from_secs(10)).await;
            unreachable!("a removal the pass gave up on must never be waited out")
        } else {
            Ok(claims_gone())
        }
    })
    .await;

    assert_eq!(settled, vec![on_time], "the pass gives up on the stall and goes on");
    assert!(mark_set(&pool, stalled_pod).await, "the stalled stop keeps its mark for the retry");
    assert!(!mark_set(&pool, on_time_pod).await, "and the next candidate settles");
    assert!(removal_notices(&pool, stalled).await.is_empty(), "a removal given up on claims nothing");
}
