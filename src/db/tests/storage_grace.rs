//! SME-345: the storage-grace pass's `sandbox_pods` queries — the quiet
//! stop's mark, the due list, the re-check — against the database, no
//! cluster.
//!
//! The clock is faked the way SME-78's tests fake theirs: an `UPDATE`
//! that moves `quietly_stopped_at` back.

use super::*;

/// `pod_id`'s mark, read back.
async fn mark(pool: &PgPool, pod_id: i64) -> Option<chrono::NaiveDateTime> {
    sqlx::query_scalar("SELECT quietly_stopped_at FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_one(pool)
        .await
        .expect("read quietly_stopped_at")
}

/// A pod's row is stamped with the database's `now()`, and stays stamped
/// until something clears it (the pass, whose clear is commit 3's).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_marking_a_pod_quietly_stopped_stamps_now(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    assert!(mark(&pool, pod.id).await.is_none(), "a live row is unmarked");

    mark_pod_quietly_stopped(&pool, pod.id).await.expect("mark it");

    let stamped = mark(&pool, pod.id).await.expect("the mark");
    let now = sqlx::query_scalar::<_, chrono::NaiveDateTime>("SELECT now()::timestamp")
        .fetch_one(&pool)
        .await
        .expect("the database's now()");
    let age = now - stamped;
    assert!(
        age.num_seconds() >= 0 && age.num_seconds() < 60,
        "the mark is now(), not a stray time: {stamped} vs {now}"
    );
}

/// A conversation with one terminated, unmarked pod row: a quiet stop's
/// mark, aged `days` back, turns it into a candidate.
async fn stopped_pod(pool: &PgPool) -> (i64, i64) {
    let conversation = create_conversation(pool).await.expect("conversation");
    let pod = create_sandbox_pod(pool, conversation.id).await.expect("pod");
    sqlx::query("UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1")
        .bind(pod.id)
        .execute(pool)
        .await
        .expect("close the row");
    (conversation.id, pod.id)
}

/// Closes and marks `pod_id`'s row `days` ago, as a quiet stop that has
/// aged would have left it.
async fn quiet_stopped_ago(pool: &PgPool, pod_id: i64, days: i32) {
    sqlx::query("UPDATE sandbox_pods SET terminated_at = now(), quietly_stopped_at = now() - make_interval(days => $2) WHERE id = $1")
        .bind(pod_id)
        .bind(days)
        .execute(pool)
        .await
        .expect("quiet-stop the row");
}

async fn candidates(pool: &PgPool, days: u32) -> Vec<(i64, i64)> {
    storage_removal_candidates(pool, days).await.expect("candidates")
}

/// The rule's edge: only marked rows qualify; the grace is `days` long,
/// not `days` plus a hair; and `0` is off, not due-for-all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_candidates_are_marked_rows_past_their_days(pool: PgPool) {
    let (conversation, pod) = stopped_pod(&pool).await;
    assert!(candidates(&pool, 7).await.is_empty(), "an unmarked row is never a candidate");

    quiet_stopped_ago(&pool, pod, 0).await;
    assert!(candidates(&pool, 7).await.is_empty(), "a fresh mark is the grace beginning, not a due date");
    quiet_stopped_ago(&pool, pod, 6).await;
    assert!(candidates(&pool, 7).await.is_empty(), "6 days old at 7 is still grace");
    quiet_stopped_ago(&pool, pod, 8).await;
    assert_eq!(candidates(&pool, 7).await, vec![(conversation, pod)]);
    assert!(candidates(&pool, 0).await.is_empty(), "0 turns removal off, it is not due-for-all");
}

/// Only the conversation's latest row can qualify: any newer row — the
/// live pod of a pickup, or a user-stopped pod whose NULL mark says keep
/// — takes the row out of the list. A deleted conversation leaves nothing
/// to sweep (its rows cascade).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_newer_row_takes_the_conversation_out_of_the_list(pool: PgPool) {
    let (conversation, pod) = stopped_pod(&pool).await;
    quiet_stopped_ago(&pool, pod, 8).await;
    assert_eq!(candidates(&pool, 7).await, vec![(conversation, pod)]);

    let newer = create_sandbox_pod(&pool, conversation).await.expect("a newer live row");
    assert!(candidates(&pool, 7).await.is_empty(), "a live pod means the conversation picked up again");
    sqlx::query("UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1")
        .bind(newer.id)
        .execute(&pool)
        .await
        .expect("close the newer row");
    assert!(
        candidates(&pool, 7).await.is_empty(),
        "a user Stop's NULL mark means keep: only the pass's quiet stop qualifies"
    );

    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(conversation)
        .execute(&pool)
        .await
        .expect("delete the conversation");
    assert!(candidates(&pool, 7).await.is_empty(), "a deleted conversation leaves nothing to remove");
}

/// The re-check answers until the clear retires the row; `0` never asks
/// the database to sweep anything.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_still_due_holds_until_the_mark_is_cleared(pool: PgPool) {
    let (_conversation, pod) = stopped_pod(&pool).await;
    quiet_stopped_ago(&pool, pod, 8).await;
    assert!(storage_removal_still_due(&pool, pod, 7).await.expect("due"));
    assert!(!storage_removal_still_due(&pool, pod, 9).await.expect("not yet due"), "9 days wanted, 8 marked");
    assert!(!storage_removal_still_due(&pool, pod, 0).await.expect("off"), "0 is off, never due");

    clear_pod_quiet_stop_mark(&pool, pod).await.expect("clear the mark");
    assert!(!storage_removal_still_due(&pool, pod, 7).await.expect("cleared"), "a cleared row never qualifies again");
    assert!(mark(&pool, pod).await.is_none(), "the clear is a NULL, not a time");
}

/// The upgrade shape: `quietly_stopped_at` is added nullable with no
/// default, so a row written before the migration — and every stopped
/// row the upgrade finds already there — keeps NULL and is never swept.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_quietly_stopped_at_is_added_nullable_with_no_default(pool: PgPool) {
    let (nullable, default): (String, Option<String>) = sqlx::query_as(
        "SELECT is_nullable, column_default FROM information_schema.columns
          WHERE table_name = 'sandbox_pods' AND column_name = 'quietly_stopped_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("the column's shape");
    assert_eq!(nullable, "YES", "NULL for rows the upgrade didn't write");
    assert_eq!(default, None, "no default that could stamp a row retroactively");
}

/// SME-123 DR9: the Sandboxes page's stored volumes say when each
/// conversation's sandbox stopped and whether that was a quiet stop: its
/// latest pod row, once that row has ended. A live pod, and a
/// conversation that never had one, give nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_conversations_last_stop_is_its_latest_ended_pod(pool: PgPool) {
    let quiet = create_conversation(&pool).await.expect("conversation");
    let old = create_sandbox_pod(&pool, quiet.id).await.expect("pod");
    terminate_sandbox_pod(&pool, old.id).await.expect("end it");
    let latest = create_sandbox_pod(&pool, quiet.id).await.expect("pod");
    mark_pod_quietly_stopped(&pool, latest.id).await.expect("mark it");
    terminate_sandbox_pod(&pool, latest.id).await.expect("end it");

    let stopped_by_user = create_conversation(&pool).await.expect("conversation");
    let pod = create_sandbox_pod(&pool, stopped_by_user.id).await.expect("pod");
    terminate_sandbox_pod(&pool, pod.id).await.expect("end it");

    let live = create_conversation(&pool).await.expect("conversation");
    create_sandbox_pod(&pool, live.id).await.expect("a live pod");

    let (stops, _) = last_pod_stops(&pool).await.expect("read");
    assert!(stops.get(&quiet.id).is_some_and(|(_, quiet_stop)| quiet_stop.is_some()), "{stops:?}");
    assert!(stops.get(&stopped_by_user.id).is_some_and(|(_, quiet_stop)| quiet_stop.is_none()), "{stops:?}");
    assert!(!stops.contains_key(&live.id), "a live pod hasn't stopped: {stops:?}");
}

/// `pod_id`'s `stopped_by_user`, read back (SME-521).
async fn stopped_by_user(pool: &PgPool, pod_id: i64) -> Option<bool> {
    sqlx::query_scalar("SELECT stopped_by_user FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_one(pool)
        .await
        .expect("read stopped_by_user")
}

/// SME-521: every way a row is closed says whether the user's Stop closed
/// it, so the storage-grace rule can give a Stop its own grace. A live row
/// says neither, and a `Plain` close after a Stop (a failed create's
/// cleanup inside SME-122's B1 window) must not turn the Stop into
/// something else.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_pod_end_records_whether_the_user_stopped_it(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let plain = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    assert_eq!(stopped_by_user(&pool, plain.id).await, None, "a live row says neither");
    terminate_sandbox_pod_as(&pool, plain.id, PodEnd::Plain).await.expect("plain end");
    assert_eq!(stopped_by_user(&pool, plain.id).await, Some(false), "terminate_pod, a lost pod, a failed create");

    let quiet = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, quiet.id, PodEnd::Quiet).await.expect("quiet end");
    assert_eq!(stopped_by_user(&pool, quiet.id).await, Some(false), "an idle or finish stop");

    let by_user = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, by_user.id, PodEnd::ByUser).await.expect("the user's Stop");
    assert_eq!(stopped_by_user(&pool, by_user.id).await, Some(true), "the user's Stop");
    terminate_sandbox_pod_as(&pool, by_user.id, PodEnd::Plain).await.expect("a plain close after it");
    assert_eq!(stopped_by_user(&pool, by_user.id).await, Some(true), "a later plain close keeps the Stop");

    let quiet_then_user = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    terminate_sandbox_pod_as(&pool, quiet_then_user.id, PodEnd::Quiet).await.expect("quiet end");
    terminate_sandbox_pod_as(&pool, quiet_then_user.id, PodEnd::ByUser).await.expect("then the user's Stop");
    assert_eq!(stopped_by_user(&pool, quiet_then_user.id).await, Some(true), "a Stop landing after a quiet stop wins (B3)");
}

/// SME-354 B14: a stop's age is measured on the database's clock, the one
/// its `terminated_at` was written on. The connection's time zone puts
/// that clock 14 hours from the server's UTC, so a mixed reading is off
/// by hours rather than by a drift too small to see.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_stops_come_with_the_databases_own_clock(pool: PgPool) {
    // Set after connecting: sqlx sends `TimeZone=UTC` at startup, which
    // overrides a `-c timezone` given in the connect options.
    let shifted = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET TIME ZONE 'Pacific/Kiritimati'").await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("a +14 pool");
    let offset: String = sqlx::query_scalar("SELECT current_setting('TimeZone')").fetch_one(&shifted).await.expect("tz");
    assert_eq!(offset, "Pacific/Kiritimati", "the session's clock is 14 hours from UTC");
    let conversation = create_conversation(&shifted).await.expect("conversation");
    let pod = create_sandbox_pod(&shifted, conversation.id).await.expect("pod");
    terminate_sandbox_pod(&shifted, pod.id).await.expect("end it");

    let (stops, observed_at) = last_pod_stops(&shifted).await.expect("read");
    let (stopped, _) = stops.get(&conversation.id).copied().expect("its stop");
    let age = observed_at - stopped;
    assert!(age >= chrono::Duration::zero() && age < chrono::Duration::minutes(1), "stopped {stopped}, observed {observed_at}: {age}");
    shifted.close().await;
}
