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
