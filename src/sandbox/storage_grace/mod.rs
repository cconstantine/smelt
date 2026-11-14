//! The storage-grace pass (SME-345, claim-driven since SME-521): once an
//! hour it lists this instance's conversation claims in the cluster, and
//! for each conversation holding some, the one rule (`rule::fate`) says
//! from the database whether its storage is in use, kept, or due. Due
//! storage — both claims, `/workspace` and Docker's data — goes the way
//! the Pods page's "remove storage" sends it, and a conversation whose pod
//! ran gets one saved notice. The clock is the latest pod row's
//! `terminated_at`, whatever ended it, or the oldest claim's creation for
//! a conversation that never had a row; a pod started within the grace
//! makes a newer, live row, so a pickup restarts the clock. A default
//! grace of `0` turns the pass off.
//!
//! The quiet stop's pass shape: hourly (a days-long grace needs no finer
//! tick, and sleeping first keeps startup's cluster work free of the
//! first-deploy sweep), skip while a conversation is busy, re-check under
//! both locks, at most `STORAGE_REMOVALS_PER_PASS` removals a pass, and
//! the list and the removal injected so the settle rules have tests
//! without a cluster.

use chrono::NaiveDateTime;

use super::*;

mod claims;
mod rule;
mod settings;

pub(crate) use self::claims::*;
pub(crate) use self::rule::*;
pub(crate) use self::settings::*;

/// How often the pass runs: an hour. A conversation crosses its grace up
/// to an hour before a pass sees it; against a grace of days that is
/// noise.
const STORAGE_REMOVAL_PASS_SECS: u64 = 3600;

/// The most conversations one pass removes (SME-521): bounds the first
/// pass after a deploy, when every stop that was never marked comes due
/// at once, and the local-path helper pods each claim's delete starts.
/// The oldest go first; the rest wait for the next hour.
pub(crate) const STORAGE_REMOVALS_PER_PASS: usize = 20;

/// How long one conversation's removal may take before the pass gives up
/// on it: like the quiet stop's delete, it runs holding that
/// conversation's turn lock, so a stalled API server must not hold the
/// user's next message for kube-rs's own, much longer, timeouts
/// (SME-78 review 1, A3's shape; SME-345 review 1, F1). 90 s covers the
/// bounded parts (the 30 s pod wait, the 5 s confirm, the deletes);
/// giving up is retry-safe — `_locked` is re-entrant and the settle rules
/// are written around the retry.
pub(crate) const STORAGE_REMOVAL_TIMEOUT: Duration = Duration::from_secs(90);

/// For as long as smelt runs, once an hour, removes the storage whose
/// grace has run out. Only a real server runs this (from `main`), never
/// the browser-test harness. The settings are read once, here.
pub async fn remove_expired_storage(pool: PgPool) {
    let settings = grace_settings();
    loop {
        tokio::time::sleep(Duration::from_secs(STORAGE_REMOVAL_PASS_SECS)).await;
        remove_expired_storage_once_with(
            &pool,
            &settings,
            STORAGE_REMOVAL_TIMEOUT,
            || list_stored_claims(&pool),
            |conversation_id| remove_conversation_storage_locked(&pool, conversation_id),
        )
        .await;
    }
}

/// One conversation the pass found due.
#[derive(Clone, Copy, Debug)]
pub(in crate::sandbox) struct Due {
    pub conversation_id: i64,
    /// Its latest pod row when listed; `None`: it never had one.
    pub pod_id: Option<i64>,
    /// Its oldest claim's creation, UTC: the clock when `pod_id` is `None`.
    pub oldest_claim_utc: Option<NaiveDateTime>,
    pub at: NaiveDateTime,
    pub days: u32,
    pub class: GraceClass,
}

/// The rule's input from a database row.
fn grace_input(row: &db::StorageGraceRow) -> GraceInput {
    GraceInput {
        latest: row.pod_id.map(|pod_id| LatestPod {
            pod_id,
            terminated_at: row.terminated_at,
            stopped_by_user: row.stopped_by_user,
            quiet: row.quiet,
        }),
        agent_done: row.agent_done,
    }
}

/// `conversation_id`'s fate from `inputs`, its claims' oldest creation
/// (UTC) put on the database's clock; `None` when the conversation is gone.
/// The Pods page asks it too, so the page and the pass can't disagree.
pub(crate) fn fate_of(
    inputs: &db::StorageGraceInputs,
    conversation_id: i64,
    oldest_claim_utc: Option<NaiveDateTime>,
    settings: &GraceSettings,
) -> Option<StorageFate> {
    let row = inputs.rows.get(&conversation_id)?;
    let oldest = oldest_claim_utc.map_or(inputs.observed_at, |at| inputs.from_utc(at));
    Some(fate(&grace_input(row), oldest, settings))
}

/// One pass: list the claims, decide each conversation's fate, log the
/// counts, and remove the due ones, oldest first, at most
/// `STORAGE_REMOVALS_PER_PASS`. Each removal is skipped while its turn
/// runs or is queued, or while a start holds its lock, re-checked under
/// both locks, and given up on past `removal_timeout` (F1's bound; a
/// test's is shorter than production's). The conversations it settled,
/// for the tests.
pub(crate) async fn remove_expired_storage_once_with<L, LFut, R, Fut>(
    pool: &PgPool,
    settings: &GraceSettings,
    removal_timeout: Duration,
    list: L,
    remove: R,
) -> Vec<i64>
where
    L: Fn() -> LFut,
    LFut: std::future::Future<Output = Result<StoredClaims, String>>,
    R: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<StorageRemoval, SandboxError>>,
{
    if settings.default_days == 0 {
        return Vec::new();
    }
    let claims = match list().await {
        Ok(claims) => claims,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list the stored claims; the storage grace pass is skipped");
            return Vec::new();
        }
    };
    let listed: Vec<(i64, Option<NaiveDateTime>)> = claims
        .by_conversation
        .iter()
        .filter(|(_, set)| !set.all_deleting)
        .map(|(id, set)| (*id, set.oldest))
        .collect();
    let ids: Vec<i64> = listed.iter().map(|(id, _)| *id).collect();
    let inputs = match db::storage_grace_inputs(pool, &ids).await {
        Ok(inputs) => inputs,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the storage grace inputs; the storage grace pass is skipped");
            return Vec::new();
        }
    };
    let (mut live, mut kept, mut waiting, mut orphaned) = (0, 0, 0, 0);
    let mut due = Vec::new();
    for (conversation_id, oldest_claim_utc) in listed {
        match fate_of(&inputs, conversation_id, oldest_claim_utc, settings) {
            // Gone: the startup sweep's (`sweep_orphaned_conversation_claims`).
            None => orphaned += 1,
            Some(StorageFate::Live) => live += 1,
            Some(StorageFate::Kept | StorageFate::Unknown) => kept += 1,
            Some(StorageFate::RemovesAt { at, .. }) if at > inputs.observed_at => waiting += 1,
            Some(StorageFate::RemovesAt { at, days, class }) => due.push(Due {
                conversation_id,
                pod_id: inputs.rows.get(&conversation_id).and_then(|row| row.pod_id),
                oldest_claim_utc,
                at,
                days,
                class,
            }),
        }
    }
    due.sort_by_key(|due| (due.at, due.conversation_id));
    let removing = due.len().min(STORAGE_REMOVALS_PER_PASS);
    tracing::info!(
        stored = claims.by_conversation.len(),
        live,
        kept,
        waiting,
        due = due.len(),
        orphaned,
        removing,
        "storage grace pass"
    );
    let remove = &remove;
    let mut settled = Vec::new();
    for due in due.into_iter().take(STORAGE_REMOVALS_PER_PASS) {
        let conversation_id = due.conversation_id;
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
            if !still_due(pool, settings, &due).await {
                return false;
            }
            match due.pod_id {
                Some(pod_id) => settle_pod_removal(pool, removal_timeout, remove, &due, pod_id).await,
                None => settle_podless_removal(removal_timeout, remove, conversation_id).await,
            }
        });
        if done.await.unwrap_or(false) {
            settled.push(conversation_id);
        }
    }
    settled
}

/// The re-check under both locks: the conversation still exists, its
/// latest pod row is the one listed (or there's still none), and the rule
/// still says due. A pod started since the list, an agent woken from
/// `finished`, or a deleted conversation each make it false.
pub(in crate::sandbox) async fn still_due(pool: &PgPool, settings: &GraceSettings, due: &Due) -> bool {
    let conversation_id = due.conversation_id;
    let inputs = match db::storage_grace_inputs(pool, &[conversation_id]).await {
        Ok(inputs) => inputs,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't re-check a conversation's storage grace");
            return false;
        }
    };
    let Some(row) = inputs.rows.get(&conversation_id) else { return false };
    if row.pod_id != due.pod_id {
        return false;
    }
    matches!(
        fate_of(&inputs, conversation_id, due.oldest_claim_utc, settings),
        Some(StorageFate::RemovesAt { at, .. }) if at <= inputs.observed_at
    )
}

/// A no-row conversation's removal (SME-521): no pod ever ran, so there is
/// no attempt to record and no notice — the model lost nothing it used. A
/// failure or a claim left behind is retried by the next pass, which lists
/// the claims again.
async fn settle_podless_removal<R, Fut>(removal_timeout: Duration, remove: &R, conversation_id: i64) -> bool
where
    R: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<StorageRemoval, SandboxError>>,
{
    match tokio::time::timeout(removal_timeout, remove(conversation_id)).await {
        Err(_elapsed) => {
            tracing::warn!(conversation_id, "a storage-grace removal of claims with no pod didn't finish in time; the next pass tries again");
            false
        }
        Ok(Err(e)) => {
            tracing::warn!(conversation_id, error = %e, "a storage-grace removal of claims with no pod failed; the next pass tries again");
            false
        }
        Ok(Ok(removal)) if removal.claims_failed > 0 => {
            tracing::warn!(conversation_id, ?removal, "a storage-grace removal left a claim behind; the next pass finishes it");
            false
        }
        Ok(Ok(_)) => true,
    }
}

/// A conversation whose pod ran: record the attempt (B13), remove,
/// bounded, and settle by the outcome — an error, a claim left behind, or
/// a removal given up on keeps the claims and says nothing, and the next
/// pass retries from the top; claims gone settles with one notice (unless
/// the removal stopped a live pod, which saved its own) and a cleared mark.
async fn settle_pod_removal<R, Fut>(pool: &PgPool, removal_timeout: Duration, remove: &R, due: &Due, pod_id: i64) -> bool
where
    R: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<StorageRemoval, SandboxError>>,
{
    let conversation_id = due.conversation_id;
    // B13: recorded before the removal, so a retry of one given up on
    // after it deleted the claims still says so.
    let tried_before = match db::start_storage_removal(pool, pod_id).await {
        Ok(tried_before) => tried_before,
        Err(e) => {
            tracing::warn!(pod_id, conversation_id, error = %e, "couldn't record a storage-grace attempt; the next pass tries again");
            return false;
        }
    };
    // F1: bounded — this runs holding the conversation's turn lock; a
    // stall settles as nothing happened, and the next pass retries.
    match tokio::time::timeout(removal_timeout, remove(conversation_id)).await {
        Err(_elapsed) => {
            tracing::warn!(conversation_id, secs = removal_timeout.as_secs(), "a storage-grace removal didn't finish in time; the next pass tries again");
            false
        }
        // A claim intact (SME-316's error-reads-as-Failed counts too):
        // storage stands, no notice, and the next pass retries from the
        // top — `_locked` re-handles gone pods and claims.
        Ok(Err(e)) => {
            tracing::warn!(conversation_id, error = %e, "a storage-grace removal failed; the next pass tries again");
            // Every error returns before the claim loop: this attempt
            // deleted nothing, so a retry finding the claims gone (the
            // user removed them meanwhile) must not credit it (SME-354
            // review 1, M1) — unless an earlier attempt, given up on, may
            // have deleted them (B13): that record stands (review 2).
            if !tried_before {
                if let Err(e) = db::forget_storage_removal_attempt(pool, pod_id).await {
                    tracing::warn!(pod_id, error = %e, "couldn't forget a failed storage-grace attempt");
                }
            }
            false
        }
        Ok(Ok(removal)) if removal.claims_failed > 0 => {
            tracing::warn!(conversation_id, ?removal, "a storage-grace removal left a claim behind; the next pass finishes it");
            false
        }
        Ok(Ok(removal)) => {
            // Claims gone: deleted now, already gone, or `Foreign` —
            // retrying could never have helped. One notice, only when this
            // stop's removal deleted something — now, or in an earlier
            // attempt given up on (SME-354 B13) — the workspace itself
            // went (F2: a `Foreign` workspace stands), and stopping a live
            // pod didn't already save one.
            let deleted = removal.claims_deleted > 0 || tried_before;
            // Forgotten first (review 1, M1; review 2, L1): should the
            // clear below fail, a retry finds no earlier attempt and
            // settles with no second notice.
            if let Err(e) = db::forget_storage_removal_attempt(pool, pod_id).await {
                tracing::warn!(pod_id, error = %e, "couldn't forget a storage-grace attempt; the next pass settles it again");
                return false;
            }
            if deleted && removal.workspace_gone && !removal.pod_stopped {
                save_removal_notice(pool, pod_id, conversation_id, due.days, due.class).await;
            }
            // Bookkeeping only since SME-521: once the claims are gone the
            // conversation isn't listed again.
            match db::clear_pod_quiet_stop_mark(pool, pod_id).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(pod_id, error = %e, "couldn't clear a storage-grace mark");
                    false
                }
            }
        }
    }
}

/// `days` as words: "1 day", "7 days".
fn day_count(days: u32) -> String {
    if days == 1 { "1 day".to_string() } else { format!("{days} days") }
}

/// The notice's words for why the storage went: a finished agent's grace
/// runs from its finish, every other one from its stop.
pub(crate) fn removal_notice_text(pod_id: i64, days: u32, class: GraceClass) -> String {
    let after = match class {
        GraceClass::Agent => format!("after {} finished", day_count(days)),
        _ => format!("after {} stopped", day_count(days)),
    };
    format!(
        "Sandbox pod {pod_id}'s storage was removed {after}. \
         /workspace and Docker's images, build cache and volumes are gone; repos cloned \
         there need cloning again. The next sandbox starts fresh with empty volumes."
    )
}

/// The one notice of a grace-expired storage, saved without moving the
/// conversation, as the quiet stop saves its own: the model reads it at
/// its next turn, after the fact. No wake: a stopped conversation is
/// woken by neither the stop nor this.
async fn save_removal_notice(pool: &PgPool, pod_id: i64, conversation_id: i64, days: u32, class: GraceClass) {
    match db::conversation_exists(pool, conversation_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't check a conversation before saving its storage-removal notice");
            return;
        }
    }
    // SME-357: marked like every saved notice.
    let text = crate::api::chat::mark_notice(&removal_notice_text(pod_id, days, class));
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
