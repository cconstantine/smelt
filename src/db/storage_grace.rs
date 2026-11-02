use super::*;

// --- Storage grace (SME-345) ---
//
// The `sandbox_pods` queries for the storage-grace pass: the mark a quiet
// stop leaves, the due list it reads, and the re-check it makes under the
// locks. These queries live here, not in `sandbox.rs`, because they all
// speak about terminated rows — the section in `sandbox.rs` speaks about
// live ones. `sandbox_pods` must be aliased `p`.

/// Stamps `pod_id`'s row as ended by a quiet stop, so the storage-grace
/// pass can count its grace from that moment. Only
/// `quiet_stop::stop_locked` calls it: every other end — a user's Stop,
/// the model's `terminate_pod`, a lost pod — leaves NULL, and only the
/// pass clears it again.
pub async fn mark_pod_quietly_stopped(pool: &PgPool, pod_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sandbox_pods SET quietly_stopped_at = now() WHERE id = $1")
        .bind(pod_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Conversations whose **latest** pod row was quietly stopped `days` days
/// ago or more, as `(conversation_id, pod_id)`, oldest stop first: the
/// pass's candidate list. Nothing at `days` of 0 (off, the
/// `POD_IDLE_PAST` guard's shape). A row stops qualifying as soon as a
/// newer row exists — a pod started within the grace, or one the pass
/// itself will stamp — so a pickup restarts the clock by itself; the
/// `EXISTS` covers a conversation deleted under a still-marked row (its
/// rows cascade away, this just closes the race with the list read).
/// No new index: hourly, small table.
pub async fn storage_removal_candidates(
    pool: &PgPool,
    days: u32,
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT p.conversation_id, p.id
           FROM sandbox_pods p
          WHERE p.quietly_stopped_at IS NOT NULL
            AND $1 > 0
            AND p.quietly_stopped_at <= now() - make_interval(days => $1)
            AND NOT EXISTS (SELECT 1 FROM sandbox_pods q
                             WHERE q.conversation_id = p.conversation_id AND q.id > p.id)
            AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = p.conversation_id)
          ORDER BY p.quietly_stopped_at ASC, p.id ASC",
    )
    .bind(days_param(days))
    .fetch_all(pool)
    .await
}

/// The one-statement re-check the pass makes under the conversation's
/// turn and start locks: `pod_id`'s row is still the conversation's
/// latest, still marked, and still `days` or more past the mark. A pod
/// started since the candidate read makes it false.
pub async fn storage_removal_still_due(pool: &PgPool, pod_id: i64, days: u32) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sandbox_pods p
          WHERE p.id = $2
            AND p.quietly_stopped_at IS NOT NULL
            AND $1 > 0
            AND p.quietly_stopped_at <= now() - make_interval(days => $1)
            AND NOT EXISTS (SELECT 1 FROM sandbox_pods q
                             WHERE q.conversation_id = p.conversation_id AND q.id > p.id)
            AND EXISTS (SELECT 1 FROM conversations c WHERE c.id = p.conversation_id))",
    )
    .bind(days_param(days))
    .bind(pod_id)
    .fetch_one(pool)
    .await
}

/// Clears the mark once the pass has settled the stop: the claims are
/// gone (or were never there), so the row never qualifies again and a
/// later quiet stop — of the newer row a pickup starts — stamps its own
/// clock.
pub async fn clear_pod_quiet_stop_mark(pool: &PgPool, pod_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sandbox_pods SET quietly_stopped_at = NULL WHERE id = $1")
        .bind(pod_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `days` as the `INT` `make_interval(days => …)` takes; the setting's
/// year ceiling keeps it far inside the range.
fn days_param(days: u32) -> i32 {
    i32::try_from(days).unwrap_or(i32::MAX)
}
