use super::*;

// --- Storage grace (SME-345) ---
//
// The `sandbox_pods` queries for the storage-grace pass: the mark a quiet
// stop leaves, the due list it reads, and the re-check it makes under the
// locks. These queries live here, not in `sandbox.rs`, because they all
// speak about terminated rows — the section in `sandbox.rs` speaks about
// live ones. `sandbox_pods` must be aliased `p`.

/// Stamps `pod_id`'s row as ended by a quiet stop, so the storage-grace
/// pass can count its grace from that moment: the tests' setup. A real
/// quiet stop stamps it in the statement that closes the row
/// (`terminate_sandbox_pod_as`'s `PodEnd::Quiet`, SME-354 B3); every
/// other end — the model's `terminate_pod`, a lost pod — leaves NULL, a
/// user's Stop clears it, and otherwise only the pass clears it again.
#[cfg(test)]
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

/// Records that the pass is about to try removing `pod_id`'s mark's
/// storage (SME-354 B13), keeping the first attempt's time. Whether an
/// earlier attempt of this mark was already recorded: one given up on may
/// have deleted the claims, so a retry finding them gone still sends the
/// stop's notice.
pub async fn start_storage_removal(pool: &PgPool, pod_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "WITH old AS (SELECT storage_removal_started_at AS at FROM sandbox_pods WHERE id = $1)
         UPDATE sandbox_pods p SET storage_removal_started_at = COALESCE(old.at, now())
           FROM old WHERE p.id = $1
         RETURNING old.at IS NOT NULL",
    )
    .bind(pod_id)
    .fetch_optional(pool)
    .await
    .map(|earlier| earlier.unwrap_or(false))
}

/// Clears the mark once the pass has settled the stop: the claims are
/// gone (or were never there), so the row never qualifies again and a
/// later quiet stop — of the newer row a pickup starts — stamps its own
/// clock.
pub async fn clear_pod_quiet_stop_mark(pool: &PgPool, pod_id: i64) -> Result<(), sqlx::Error> {
    #[cfg(test)]
    if let Some(result) = fake_clear::take(pod_id) {
        return result;
    }
    sqlx::query("UPDATE sandbox_pods SET quietly_stopped_at = NULL, storage_removal_started_at = NULL WHERE id = $1")
        .bind(pod_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A clear that fails without a database failure, for the pass's test
/// that a failed clear still leaves one notice per stop (SME-345 review
/// 1, F4). Armed per pod id, the quiet stop's `fake_delete` shape
/// without its gate — this failure needs no pause, only one.
#[cfg(test)]
pub(crate) mod fake_clear {
    use std::collections::HashSet;
    use std::sync::{LazyLock, Mutex};

    static PODS: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(Default::default);

    /// The next clear of `pod_id` fails; the one after it is real.
    pub(crate) fn fail_next(pod_id: i64) {
        PODS.lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id);
    }

    pub(crate) fn take(pod_id: i64) -> Option<Result<(), sqlx::Error>> {
        PODS.lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pod_id)
            .then(|| Err(sqlx::Error::PoolClosed))
    }
}

/// `days` as the `INT` `make_interval(days => …)` takes; the setting's
/// year ceiling keeps it far inside the range.
fn days_param(days: u32) -> i32 {
    i32::try_from(days).unwrap_or(i32::MAX)
}

/// When each conversation's sandbox last stopped, and the quiet stop's
/// mark if that stop was one (SME-123 DR9: the stored volumes said
/// neither): its latest pod row, if that row has ended. Keyed by
/// conversation; a conversation whose latest pod is live has no entry.
/// With the clock the stops are measured against (SME-354 B14).
pub async fn last_pod_stops(pool: &PgPool) -> Result<(LastPodStops, chrono::NaiveDateTime), sqlx::Error> {
    // One connection for both reads: the clock is `now()` in the session
    // the timestamps are read in, never the server's own (B14).
    let mut conn = pool.acquire().await?;
    let rows: Vec<(i64, chrono::NaiveDateTime, Option<chrono::NaiveDateTime>)> = sqlx::query_as(
        "SELECT DISTINCT ON (conversation_id) conversation_id, terminated_at, quietly_stopped_at
           FROM sandbox_pods
          ORDER BY conversation_id, id DESC",
    )
    .fetch_all(&mut *conn)
    .await
    .map(|rows: Vec<(i64, Option<chrono::NaiveDateTime>, Option<chrono::NaiveDateTime>)>| {
        rows.into_iter().filter_map(|(id, ended, quiet)| ended.map(|ended| (id, ended, quiet))).collect()
    })?;
    let stops = rows.into_iter().map(|(id, ended, quiet)| (id, (ended, quiet))).collect();
    let observed_at = sqlx::query_scalar("SELECT now()::timestamp").fetch_one(&mut *conn).await?;
    Ok((stops, observed_at))
}

/// Each conversation's last stop and its quiet stop's mark, if any.
pub type LastPodStops = std::collections::HashMap<i64, (chrono::NaiveDateTime, Option<chrono::NaiveDateTime>)>;
