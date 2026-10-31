//! Stopping a sandbox pod quietly (SME-78): on idle, once a pod with no
//! running command has gone `SANDBOX_IDLE_STOP_MINUTES` with no activity,
//! and at once when a sub-agent finishes. The stop works like the user's
//! Stop: `/workspace`, Docker's data and mounted volumes are claims, so
//! they're kept; terminals and processes go. The model gets one saved
//! notice and isn't woken, and the conversation doesn't move in the
//! sidebar.
//!
//! A busy pod is never stopped: a turn running or queued, a repo cloning,
//! or any running terminal command (the user's 2026-10-31 answer: "Do not
//! stop running commands"). So no quiet stop ever ends a command, and
//! SME-79's "smelt never stops a command" stays true.
//!
//! The rule lives in SQL (`db::quiet_stop_candidates`,
//! `db::pod_still_stoppable`), so a restart doesn't reset the clock. A stop
//! takes the conversation's turn lock and then its pod-start lock, the
//! order a turn takes them, and re-checks the rule in one statement under
//! both: no turn, and so no command, can start in between, and neither can
//! a pod start or a clone.

use std::collections::HashSet;

use super::*;

/// Conversations with activity smelt saw outside the database — a preview
/// request, a pod dial, the end of a turn that brought something new — not
/// yet written to their pods' `last_active_at`. In memory, so a preview's
/// hot path writes nothing; the idle pass flushes it every minute, and
/// again before each re-check. A restart loses at most one pass of notes.
#[derive(Default)]
pub(crate) struct ActivityNotes {
    noted: StdMutex<HashSet<i64>>,
    /// Makes the next flush fail as a database error would, for the tests.
    #[cfg(test)]
    fail_next_flush: std::sync::atomic::AtomicBool,
}

impl ActivityNotes {
    pub(crate) fn note(&self, conversation_id: i64) {
        self.noted.lock().unwrap_or_else(|e| e.into_inner()).insert(conversation_id);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_flush(&self) {
        self.fail_next_flush.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, conversation_id: i64) -> bool {
        self.noted.lock().unwrap_or_else(|e| e.into_inner()).contains(&conversation_id)
    }

    /// Writes the notes to the database, and forgets them only once that
    /// succeeded: a failed flush keeps them for the next one. Whether it
    /// succeeded (nothing to write is success).
    pub(crate) async fn flush(&self, pool: &PgPool) -> bool {
        let noted: Vec<i64> = self.noted.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect();
        if noted.is_empty() {
            return true;
        }
        #[cfg(test)]
        let flushed = if self.fail_next_flush.swap(false, std::sync::atomic::Ordering::SeqCst) {
            Err(sqlx::Error::PoolTimedOut)
        } else {
            db::flush_pod_activity(pool, &noted).await
        };
        #[cfg(not(test))]
        let flushed = db::flush_pod_activity(pool, &noted).await;
        match flushed {
            Ok(()) => {
                let mut notes = self.noted.lock().unwrap_or_else(|e| e.into_inner());
                for id in &noted {
                    notes.remove(id);
                }
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "couldn't record sandbox pod activity; kept for the next pass");
                false
            }
        }
    }
}

/// The notes the server's own pass flushes. Tests that run a pass bring a
/// set of their own, so no test flushes another's notes.
static ACTIVITY: LazyLock<ActivityNotes> = LazyLock::new(ActivityNotes::default);

/// Something used `conversation_id`'s pod (or its conversation brought the
/// model something new): its idle clock starts again at the next flush.
/// Never touches the database, so it's cheap enough for every preview
/// request.
pub(crate) fn note_activity(conversation_id: i64) {
    ACTIVITY.note(conversation_id);
}

/// Whether `note_activity` noted `conversation_id` and the server's pass
/// hasn't flushed it, for the tests of its callers.
#[cfg(test)]
pub(crate) fn activity_noted(conversation_id: i64) -> bool {
    ACTIVITY.contains(conversation_id)
}

/// `SANDBOX_IDLE_STOP_MINUTES`, default 60, `0` = idle stops off; read once,
/// at the first call. The finish stop doesn't depend on it.
pub(crate) fn idle_stop_minutes() -> u32 {
    static MINUTES: OnceLock<u32> = OnceLock::new();
    *MINUTES.get_or_init(|| idle_stop_minutes_from(std::env::var("SANDBOX_IDLE_STOP_MINUTES").ok()))
}

/// The default idle threshold, an hour.
const DEFAULT_IDLE_STOP_MINUTES: u32 = 60;

/// The largest threshold that means something, a week; past it, warn and
/// use the default, as `idle_notice_secs_from` does past a day.
const MAX_IDLE_STOP_MINUTES: i64 = 7 * 24 * 60;

/// The minutes for a `SANDBOX_IDLE_STOP_MINUTES` of `setting`: unset or
/// empty is the default, `0` turns idle stops off, and anything that isn't
/// a whole number of minutes from 1 to a week logs a warning and uses the
/// default. Pulled out so the parsing is testable without the environment.
pub(super) fn idle_stop_minutes_from(setting: Option<String>) -> u32 {
    let Some(setting) = setting.filter(|s| !s.is_empty()) else {
        return DEFAULT_IDLE_STOP_MINUTES;
    };
    match setting.parse::<i64>() {
        Ok(minutes) if (0..=MAX_IDLE_STOP_MINUTES).contains(&minutes) => {
            u32::try_from(minutes).unwrap_or(DEFAULT_IDLE_STOP_MINUTES)
        }
        _ => {
            tracing::warn!(
                minutes = %setting,
                ceiling_minutes = MAX_IDLE_STOP_MINUTES,
                default_minutes = DEFAULT_IDLE_STOP_MINUTES,
                "SANDBOX_IDLE_STOP_MINUTES is not a whole number of minutes from 0 to a week; using the default instead"
            );
            DEFAULT_IDLE_STOP_MINUTES
        }
    }
}

/// Why a pod is stopped quietly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    Idle { minutes: u32 },
    Finished,
}

impl StopReason {
    fn of(kind: db::QuietStopKind, minutes: u32) -> Self {
        match kind {
            db::QuietStopKind::Idle => StopReason::Idle { minutes },
            db::QuietStopKind::Finished => StopReason::Finished,
        }
    }

    fn kind(self) -> db::QuietStopKind {
        match self {
            StopReason::Idle { .. } => db::QuietStopKind::Idle,
            StopReason::Finished => db::QuietStopKind::Finished,
        }
    }

    fn minutes(self) -> u32 {
        match self {
            StopReason::Idle { minutes } => minutes,
            StopReason::Finished => 0,
        }
    }

    /// The notice the model reads, saved as a `user` message as every
    /// notice is.
    pub(crate) fn notice(self, pod_id: i64) -> String {
        const KEPT: &str = "/workspace, Docker's images, build cache and volumes, and mounted volumes are kept; \
            terminals, background processes and containers, installed packages and files elsewhere are gone.";
        match self {
            StopReason::Idle { minutes } => format!(
                "Sandbox pod {pod_id} was stopped after {minutes} minutes with no activity and no command running. {KEPT} \
                 create_pod (or clone_repo) starts a new pod with /workspace as it was."
            ),
            StopReason::Finished => format!(
                "Sandbox pod {pod_id} was stopped because you finished. {KEPT} If you're given more work, \
                 create_pod (or clone_repo) starts a new pod with /workspace as it was."
            ),
        }
    }
}

/// How long a quiet stop waits for the cluster's part (reading the pod and
/// deleting it) before giving up: it holds the conversation's turn lock, so
/// a stalled API server mustn't hold the user's next message for kube-rs's
/// own, much longer, timeouts (SME-78 review 1, A3). Short in tests.
#[cfg(not(test))]
pub(crate) const QUIET_STOP_DELETE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
pub(crate) const QUIET_STOP_DELETE_TIMEOUT: Duration = Duration::from_millis(500);

/// The cluster's part of a quiet stop in production: the pod's delete.
pub(in crate::sandbox) async fn cluster_delete(pool: &PgPool, pod_id: i64) -> Result<(), SandboxError> {
    #[cfg(test)]
    if fake_delete::take(pod_id).await {
        return Ok(());
    }
    Box::pin(delete_terminated_pod(pool, &get()?.client, pod_id)).await
}

/// A cluster delete that succeeds without a cluster, for the tests of the
/// finish stop, which runs from `after_last_turn` with the real delete.
#[cfg(test)]
pub(crate) mod fake_delete {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use tokio::sync::oneshot;

    type Gate = Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>;
    static FAKES: LazyLock<Mutex<HashMap<i64, Gate>>> = LazyLock::new(Default::default);

    /// `pod_id`'s next delete succeeds at once.
    pub(crate) fn succeed(pod_id: i64) {
        FAKES.lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, None);
    }

    /// `pod_id`'s next delete waits: the first receiver fires once it's
    /// waiting, and sending on the second lets it succeed.
    pub(crate) fn hold(pod_id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        FAKES.lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, Some((reached_tx, release_rx)));
        (reached_rx, release_tx)
    }

    /// Whether `pod_id`'s delete is faked, after waiting at its gate.
    pub(super) async fn take(pod_id: i64) -> bool {
        let fake = FAKES.lock().unwrap_or_else(|e| e.into_inner()).remove(&pod_id);
        match fake {
            None => false,
            Some(None) => true,
            Some(Some((reached, release))) => {
                let _ = reached.send(());
                let _ = release.await;
                true
            }
        }
    }
}

/// Stops `pod_id` quietly, called with its conversation's turn lock and
/// start lock held after a re-check said yes. Closes its terminals (none
/// has a command running: the re-check said so, and none can start while
/// the turn lock is held), runs `delete` and closes the row through
/// `terminate_pod_with` (which publishes the pod's end and `PodsChanged`),
/// then saves the notice in place, still under the turn lock, so it can't
/// split a tool call from its result. A failed delete saves nothing and
/// leaves the row live for the next pass. Whether the pod was stopped.
pub(crate) async fn stop_locked(
    pool: &PgPool,
    pod_id: i64,
    conversation_id: i64,
    reason: StopReason,
    delete: impl std::future::Future<Output = Result<(), SandboxError>>,
) -> bool {
    close_pod_terminals(pool, pod_id).await;
    let delete = async {
        tokio::time::timeout(QUIET_STOP_DELETE_TIMEOUT, delete).await.unwrap_or_else(|_| {
            Err(SandboxError::Timeout(Some(format!(
                "the cluster didn't answer the pod's delete within {} s",
                QUIET_STOP_DELETE_TIMEOUT.as_secs_f32()
            ))))
        })
    };
    match terminate_pod_with(pool, pod_id, delete).await {
        Ok(Some(_)) => {}
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!(pod_id, conversation_id, error = %e, "a quiet stop's delete failed; the next pass tries again");
            return false;
        }
    }
    tracing::info!(pod_id, conversation_id, ?reason, "stopped a sandbox pod quietly");
    save_stop_notice(pool, pod_id, conversation_id, reason).await;
    true
}

/// The quiet stop's notice, saved without moving the conversation, unless
/// it was deleted after the re-check.
async fn save_stop_notice(pool: &PgPool, pod_id: i64, conversation_id: i64, reason: StopReason) {
    match db::conversation_exists(pool, conversation_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't check a conversation before saving a quiet stop's notice");
            return;
        }
    }
    let content = [crate::anthropic::ContentBlock::Text { text: reason.notice(pod_id) }];
    match db::create_message_in_place(pool, conversation_id, "user", &content).await {
        Ok(saved) => events::publish(
            conversation_id,
            events::ConversationEvent::MessagesAppended { messages: vec![saved] },
        ),
        // Deleted between the check and the insert.
        Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => {
            tracing::debug!(conversation_id, "a quiet stop's conversation was deleted before its notice");
        }
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't save a quiet stop's notice"),
    }
}

/// How often the idle pass runs: a pod stops between N and N+1 minutes
/// after its last activity.
const IDLE_STOP_TICK: Duration = Duration::from_secs(60);

/// For as long as smelt runs, stops idle pods and finished agents' pods
/// quietly, once a minute. Only a real server runs this (from `main`),
/// never the browser-test harness. The threshold is read once, here.
pub async fn stop_idle_pods(pool: PgPool) {
    let minutes = idle_stop_minutes();
    loop {
        tokio::time::sleep(IDLE_STOP_TICK).await;
        stop_idle_pods_once_with(&pool, minutes, &ACTIVITY, |pod_id| cluster_delete(&pool, pod_id)).await;
    }
}

/// One idle pass: flushes `notes`, then for each candidate — a pod idle
/// past `minutes` (none at 0), or a finished agent's (the backstop for a
/// finish stop that didn't run) — takes the locks without waiting, flushes
/// again, re-checks and stops. A pod whose turn runs, or whose lock is
/// held, is skipped and tried again next pass. The pods it stopped, for
/// the tests.
pub(crate) async fn stop_idle_pods_once_with<D, Fut>(
    pool: &PgPool,
    minutes: u32,
    notes: &ActivityNotes,
    delete: D,
) -> Vec<i64>
where
    D: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<(), SandboxError>>,
{
    notes.flush(pool).await;
    let candidates = match db::quiet_stop_candidates(pool, minutes).await {
        Ok(candidates) => candidates,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list sandbox pods to stop");
            return Vec::new();
        }
    };
    let mut stopped = Vec::new();
    for (pod_id, conversation_id, kind) in candidates {
        let reason = StopReason::of(kind, minutes);
        if stop_candidate(pool, notes, pod_id, conversation_id, reason, delete(pod_id)).await {
            stopped.push(pod_id);
        }
    }
    stopped
}

/// One candidate of the idle pass: skipped while its conversation's turn
/// runs or is queued, or while either lock is held; otherwise flushed,
/// re-checked and stopped under both locks.
pub(crate) async fn stop_candidate(
    pool: &PgPool,
    notes: &ActivityNotes,
    pod_id: i64,
    conversation_id: i64,
    reason: StopReason,
    delete: impl std::future::Future<Output = Result<(), SandboxError>>,
) -> bool {
    if crate::turn::turn_running(conversation_id) {
        return false;
    }
    let stop = crate::turn::between_turns(conversation_id, false, || async move {
        let start_lock = pod_start_lock(conversation_id);
        let Ok(_starting) = start_lock.try_lock() else {
            return false;
        };
        // A turn admitted while the lock was being taken waits behind it:
        // busy all the same.
        if crate::turn::turn_running(conversation_id) {
            return false;
        }
        // A failed flush may hide activity just noted: no stop on a stale
        // clock this pass (review 1, A1).
        if !notes.flush(pool).await {
            return false;
        }
        match db::pod_still_stoppable(pool, pod_id, reason.kind(), reason.minutes()).await {
            Ok(true) => stop_locked(pool, pod_id, conversation_id, reason, delete).await,
            Ok(false) => false,
            Err(e) => {
                tracing::warn!(pod_id, error = %e, "couldn't re-check a sandbox pod before stopping it");
                false
            }
        }
    });
    stop.await.unwrap_or(false)
}

/// The finish stop: a sub-agent's turn ended with it `finished`
/// (`agents::after_last_turn`), so its pod stops at once rather than after
/// an hour, unless a command is still running (the next idle pass's
/// backstop takes it once the command ends). No live pod — the seat ran
/// `terminate_pod` itself — is a no-op.
pub(crate) async fn stop_finished_agent_pod(pool: &PgPool, conversation_id: i64) -> bool {
    stop_finished_agent_pod_with(pool, conversation_id, |pod_id| cluster_delete(pool, pod_id)).await
}

/// `stop_finished_agent_pod` with the cluster's delete passed in. Waits for
/// the conversation's turn lock and then its start lock, the order a turn
/// takes them, and re-checks under both. A `message_agent` racing it has
/// one winner, decided by the turn lock: one that set the child `active`
/// before the re-check keeps the pod (the re-check skips); one after it
/// finds the pod stopped, and its woken turn reads the notice, then the
/// message, and starts a pod on the same claims if it needs one.
pub(crate) async fn stop_finished_agent_pod_with<D, Fut>(pool: &PgPool, conversation_id: i64, delete: D) -> bool
where
    D: FnOnce(i64) -> Fut,
    Fut: std::future::Future<Output = Result<(), SandboxError>>,
{
    let pod_id = match db::list_sandbox_pods(pool, conversation_id).await {
        Ok(pods) => match pods.first() {
            Some(pod) => pod.id,
            None => return false,
        },
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't read a finished agent's pods; the idle pass will look again");
            return false;
        }
    };
    let stop = crate::turn::between_turns(conversation_id, true, || async move {
        let start_lock = pod_start_lock(conversation_id);
        let _starting = start_lock.lock().await;
        match db::pod_still_stoppable(pool, pod_id, db::QuietStopKind::Finished, 0).await {
            Ok(true) => stop_locked(pool, pod_id, conversation_id, StopReason::Finished, delete(pod_id)).await,
            Ok(false) => false,
            Err(e) => {
                tracing::warn!(pod_id, error = %e, "couldn't re-check a finished agent's pod before stopping it");
                false
            }
        }
    });
    stop.await.unwrap_or(false)
}
