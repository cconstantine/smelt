//! The storage-grace pass (SME-345): `SANDBOX_STORAGE_REMOVAL_DAYS` days
//! after a pod was stopped quietly, its conversation's two claims —
//! `/workspace` and Docker's data — go the way the Pods page's "remove
//! storage" sends them, and the model gets one saved notice. Only quiet
//! stops are ever marked, so a conversation the user stopped keeps its
//! storage until it says otherwise; a pod started within the grace makes
//! a newer row, and the old mark stops counting (a pickup restarts the
//! clock). `0` turns the pass off.
//!
//! The quiet stop's pass shape: hourly (a days-long grace needs no finer
//! tick, and sleeping first keeps startup's cluster work free of the
//! first-deploy sweep), skip while a conversation is busy, re-check in
//! one statement under both locks, and the removal injected so the
//! settle rules have tests without a cluster.

use super::*;

/// `SANDBOX_STORAGE_REMOVAL_DAYS`, default 7, `0` = automatic removal
/// off; read once when the pass task starts, like `idle_stop_minutes`
/// at the first call. Changing it needs a restart.
pub(crate) fn storage_removal_days() -> u32 {
    static DAYS: OnceLock<u32> = OnceLock::new();
    *DAYS.get_or_init(|| storage_removal_days_from(std::env::var("SANDBOX_STORAGE_REMOVAL_DAYS").ok()))
}

/// The default grace period, a week.
const DEFAULT_SANDBOX_STORAGE_REMOVAL_DAYS: u32 = 7;

/// The largest grace that means something, a year: past it, warn and use
/// the default — the ceiling is what catches a value written in seconds
/// (`604800`) and answers it with a week, not ~1,657 years.
const MAX_SANDBOX_STORAGE_REMOVAL_DAYS: i64 = 365;

/// The days for a `SANDBOX_STORAGE_REMOVAL_DAYS` of `setting`: unset or
/// empty is the default, `0` turns automatic removal off, and anything
/// that isn't a whole number of days from 1 to a year logs a warning and
/// uses the default. Pulled out so the parsing is testable without the
/// environment.
pub(super) fn storage_removal_days_from(setting: Option<String>) -> u32 {
    let Some(setting) = setting.filter(|s| !s.is_empty()) else {
        return DEFAULT_SANDBOX_STORAGE_REMOVAL_DAYS;
    };
    match setting.parse::<i64>() {
        Ok(days) if (0..=MAX_SANDBOX_STORAGE_REMOVAL_DAYS).contains(&days) => {
            u32::try_from(days).unwrap_or(DEFAULT_SANDBOX_STORAGE_REMOVAL_DAYS)
        }
        _ => {
            tracing::warn!(
                days = %setting,
                ceiling_days = MAX_SANDBOX_STORAGE_REMOVAL_DAYS,
                default_days = DEFAULT_SANDBOX_STORAGE_REMOVAL_DAYS,
                "SANDBOX_STORAGE_REMOVAL_DAYS is not a whole number of days from 0 to a year; using the default instead"
            );
            DEFAULT_SANDBOX_STORAGE_REMOVAL_DAYS
        }
    }
}

/// How often the pass runs: an hour. A conversation crosses its grace up
/// to an hour before a pass sees it; against a grace of days that is
/// noise.
const STORAGE_REMOVAL_PASS_SECS: u64 = 3600;

/// How long one conversation's removal may take before the pass gives up
/// on it: like the quiet stop's delete, it runs holding that
/// conversation's turn lock, so a stalled API server must not hold the
/// user's next message for kube-rs's own, much longer, timeouts
/// (SME-78 review 1, A3's shape; SME-345 review 1, F1). 90 s covers the
/// bounded parts (the 30 s pod wait, the 5 s confirm, the deletes);
/// giving up is retry-safe — `_locked` is re-entrant and the settle rules
/// are written around the retry.
pub(crate) const STORAGE_REMOVAL_TIMEOUT: Duration = Duration::from_secs(90);

/// For as long as smelt runs, once an hour, removes the storage of
/// conversations quiet-stopped `days` or more ago (nothing at 0). Only a
/// real server runs this (from `main`), never the browser-test harness.
/// The setting is read once, here.
pub async fn remove_expired_storage(pool: PgPool) {
    let days = storage_removal_days();
    loop {
        tokio::time::sleep(Duration::from_secs(STORAGE_REMOVAL_PASS_SECS)).await;
        remove_expired_storage_once_with(&pool, days, STORAGE_REMOVAL_TIMEOUT, |conversation_id| {
            remove_conversation_storage_locked(&pool, conversation_id)
        })
        .await;
    }
}

/// One pass: for each due conversation — skipped while its turn runs or
/// is queued, or while a start holds its lock — re-check under both locks
/// and remove, with each removal given up on past `removal_timeout` (F1's
/// bound; a test's is shorter than production's), then settle by the
/// outcome: an error, a claim left behind, or a removal given up on keeps
/// the mark, no notice, and the next pass retries from the top; claims
/// gone settles with one notice (unless the removal stopped a live pod,
/// which saved its own) and a cleared mark. The conversations it settled,
/// for the tests.
pub(crate) async fn remove_expired_storage_once_with<R, Fut>(
    pool: &PgPool,
    days: u32,
    removal_timeout: Duration,
    remove: R,
) -> Vec<i64>
where
    R: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<StorageRemoval, SandboxError>>,
{
    if days == 0 {
        return Vec::new();
    }
    let candidates = match db::storage_removal_candidates(pool, days).await {
        Ok(candidates) => candidates,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list conversations whose storage grace ran out");
            return Vec::new();
        }
    };
    // Called once per candidate without consuming `remove`: the settle's
    // future only ever needs a borrow of it (as in the quiet stop's pass).
    let remove = &remove;
    let mut settled = Vec::new();
    for (conversation_id, pod_id) in candidates {
        if crate::turn::turn_running(conversation_id) {
            continue;
        }
        let done = crate::turn::between_turns(conversation_id, false, || async move {
            let start_lock = pod_start_lock(conversation_id);
            let Ok(_starting) = start_lock.try_lock() else {
                return false;
            };
            // A turn admitted while the lock was being taken: busy all
            // the same, as in the quiet stop's candidate.
            if crate::turn::turn_running(conversation_id) {
                return false;
            }
            match db::storage_removal_still_due(pool, pod_id, days).await {
                Ok(true) => {}
                // A pod started since the list read: the clock restarted.
                Ok(false) => return false,
                Err(e) => {
                    tracing::warn!(pod_id, conversation_id, error = %e, "couldn't re-check a conversation's storage grace");
                    return false;
                }
            }
            // F1: bounded — this runs holding the conversation's turn
            // lock; a stall settles as nothing happened, and the next
            // pass retries from the top.
            let outcome = tokio::time::timeout(removal_timeout, remove(conversation_id)).await;
            match outcome {
                Err(_elapsed) => {
                    tracing::warn!(conversation_id, secs = removal_timeout.as_secs(), "a storage-grace removal didn't finish in time; the next pass tries again");
                    false
                }
                // A claim intact (SME-316's error-reads-as-Failed counts
                // too): storage stands, no notice, and the next pass
                // retries from the top — `_locked` re-handles gone pods
                // and claims.
                Ok(Err(e)) => {
                    tracing::warn!(conversation_id, error = %e, "a storage-grace removal failed; the next pass tries again");
                    false
                }
                Ok(Ok(removal)) if removal.claims_failed > 0 => {
                    tracing::warn!(conversation_id, ?removal, "a storage-grace removal left a claim behind; the next pass finishes it");
                    false
                }
                Ok(Ok(removal)) => {
                    // Claims gone: deleted now, already gone (the user
                    // removed the storage after the stop), or `Foreign`
                    // — retrying could never have helped. One notice,
                    // only when this removal deleted something and
                    // stopping a live pod didn't already save one.
                    if removal.claims_deleted > 0 && !removal.pod_stopped {
                        save_removal_notice(pool, pod_id, conversation_id, days).await;
                    }
                    // If clearing fails the next pass re-removes as a
                    // no-op (claims gone now settles without a notice),
                    // so one notice per stop still holds.
                    match db::clear_pod_quiet_stop_mark(pool, pod_id).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(pod_id, error = %e, "couldn't clear a storage-grace mark; the next pass settles it again");
                            false
                        }
                    }
                }
            }
        });
        if done.await.unwrap_or(false) {
            settled.push(conversation_id);
        }
    }
    settled
}

/// The one notice of a grace-expired storage, saved without moving the
/// conversation, as the quiet stop saves its own: the model reads it at
/// its next turn, after the fact. No wake: a stopped conversation is
/// woken by neither the stop nor this.
async fn save_removal_notice(pool: &PgPool, pod_id: i64, conversation_id: i64, days: u32) {
    match db::conversation_exists(pool, conversation_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't check a conversation before saving its storage-removal notice");
            return;
        }
    }
    let text = format!(
        "Sandbox pod {pod_id}'s storage was removed after {days} days stopped. \
         /workspace and Docker's images, build cache and volumes are gone; repos cloned \
         there need cloning again. The next sandbox starts fresh with empty volumes."
    );
    let content = [crate::anthropic::ContentBlock::Text { text }];
    match db::create_message_in_place(pool, conversation_id, "user", &content).await {
        Ok(saved) => events::publish(
            conversation_id,
            events::ConversationEvent::MessagesAppended { messages: vec![saved] },
        ),
        // Deleted between the check and the insert.
        Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => {
            tracing::debug!(conversation_id, "a storage-grace conversation was deleted before its notice");
        }
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't save a storage-grace notice"),
    }
}
