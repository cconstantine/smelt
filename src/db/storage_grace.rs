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
/// user's Stop clears it, so does the user's storage removal (SME-354
/// review 1, M1), and otherwise only the pass clears it again.
#[cfg(test)]
pub async fn mark_pod_quietly_stopped(pool: &PgPool, pod_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sandbox_pods SET quietly_stopped_at = now() WHERE id = $1")
        .bind(pod_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// What the storage-grace rule reads about one conversation (SME-521): its
/// latest pod row, if any, and whether it is a sub-agent that's done.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct StorageGraceRow {
    pub conversation_id: i64,
    /// The latest `sandbox_pods` row's id; `None` when it never had one.
    pub pod_id: Option<i64>,
    pub terminated_at: Option<NaiveDateTime>,
    pub stopped_by_user: Option<bool>,
    /// The latest row carries a quiet stop's mark.
    pub quiet: bool,
    /// `agents.state` is `finished`, `stopped` or `failed`.
    pub agent_done: bool,
    /// For a done agent only, the later of its latest message and a
    /// question still waiting on the user (SME-521 review 1, M1).
    pub activity_at: Option<NaiveDateTime>,
}

/// The rule's inputs for a set of conversations, with the database's
/// clock (SME-354 B14). A conversation that no longer exists has no row.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageGraceInputs {
    pub rows: std::collections::HashMap<i64, StorageGraceRow>,
    /// `now()` in the session the timestamps were read in.
    pub observed_at: NaiveDateTime,
    /// That session's clock less UTC's, so a claim's UTC creation time can
    /// be put on the same clock.
    pub utc_offset: chrono::Duration,
}

impl StorageGraceInputs {
    /// A UTC time (a claim's `creationTimestamp`) on this read's clock.
    pub fn from_utc(&self, at: NaiveDateTime) -> NaiveDateTime {
        at + self.utc_offset
    }
}

/// The storage-grace inputs of `conversation_ids` (SME-521): each one's
/// latest pod row (`DISTINCT ON`'s shape, by the highest id), its agent
/// state and, for a done agent, its last activity, read in one statement on one connection with the clock.
/// No new index: hourly, and keyed by conversation.
pub async fn storage_grace_inputs(pool: &PgPool, conversation_ids: &[i64]) -> Result<StorageGraceInputs, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let rows: Vec<StorageGraceRow> = sqlx::query_as(
        "SELECT c.id AS conversation_id, p.id AS pod_id, p.terminated_at, p.stopped_by_user,
                COALESCE(p.quietly_stopped_at IS NOT NULL, false) AS quiet,
                COALESCE(a.state IN ('finished', 'stopped', 'failed'), false) AS agent_done,
                CASE WHEN a.state IN ('finished', 'stopped', 'failed') THEN GREATEST(
                  (SELECT max(m.created_at) FROM messages m WHERE m.conversation_id = c.id),
                  (SELECT q.created_at FROM pending_questions q
                    WHERE q.conversation_id = c.id AND q.answer IS NULL AND q.asked_of = 'user'))
                END AS activity_at
           FROM conversations c
           LEFT JOIN LATERAL (SELECT q.id, q.terminated_at, q.stopped_by_user, q.quietly_stopped_at
                                FROM sandbox_pods q WHERE q.conversation_id = c.id
                               ORDER BY q.id DESC LIMIT 1) p ON true
           LEFT JOIN agents a ON a.conversation_id = c.id
          WHERE c.id = ANY($1)",
    )
    .bind(conversation_ids)
    .fetch_all(&mut *conn)
    .await?;
    let (observed_at, utc_offset_secs): (NaiveDateTime, f64) = sqlx::query_as(
        "SELECT now()::timestamp, EXTRACT(EPOCH FROM (now()::timestamp - (now() AT TIME ZONE 'UTC')))::float8",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(StorageGraceInputs {
        rows: rows.into_iter().map(|row| (row.conversation_id, row)).collect(),
        observed_at,
        utc_offset: chrono::Duration::seconds(utc_offset_secs.round() as i64),
    })
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

/// Forgets this mark's recorded attempt (SME-354 review 1, M1), for the
/// two outcomes that leave nothing for a retry to announce: a first
/// attempt that failed before deleting anything (a later one leaves an
/// earlier attempt's record standing), and a settled one, before its
/// notice and clear. A retry after a failed clear then finds no earlier
/// attempt and settles quietly, so the stop keeps its one notice.
pub async fn forget_storage_removal_attempt(pool: &PgPool, pod_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sandbox_pods SET storage_removal_started_at = NULL WHERE id = $1")
        .bind(pod_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Settles every quiet stop of `conversation_id` (SME-354 review 1, M1):
/// the user removed its storage, so the grace pass has nothing left to
/// remove, and must not later read the claims' absence as its own work.
pub async fn clear_conversation_quiet_stop_marks(pool: &PgPool, conversation_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE sandbox_pods SET quietly_stopped_at = NULL, storage_removal_started_at = NULL
          WHERE conversation_id = $1 AND (quietly_stopped_at IS NOT NULL OR storage_removal_started_at IS NOT NULL)",
    )
    .bind(conversation_id)
    .execute(pool)
    .await?;
    Ok(())
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
