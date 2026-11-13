use super::*;

mod commands;
pub use commands::*;

// --- Terminal (pod/terminal/command lifecycle) ---
// Server-only, no client/server boundary to cross (no UI yet) — unlike
// Conversation/Message, these don't need to live in models.rs or derive
// anything WASM-relevant. See SME-9.

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct SandboxPod {
    pub id: i64,
    pub conversation_id: i64,
    pub created_at: NaiveDateTime,
    pub terminated_at: Option<NaiveDateTime>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct SandboxTerminal {
    pub id: i64,
    pub pod_id: i64,
    pub created_at: NaiveDateTime,
    pub terminated_at: Option<NaiveDateTime>,
}

pub async fn create_sandbox_pod(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<SandboxPod, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "INSERT INTO sandbox_pods (conversation_id) VALUES ($1) RETURNING *",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
}

/// Idempotent by design (unlike creation) — terminating an already-
/// terminated pod converges on the same end state, so it's safe to call
/// without first checking. A `pod_id` that doesn't exist at all is a
/// separate, real error, surfaced by the caller checking the row count
/// via `RETURNING` returning nothing — see `sandbox.rs`.
pub async fn terminate_sandbox_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<SandboxPod>, sqlx::Error> {
    terminate_sandbox_pod_as(pool, pod_id, PodEnd::Plain).await
}

/// How a pod's row is closed (SME-354 B3). A user's Stop takes neither
/// the turn lock nor the start lock, so it can land inside a quiet stop
/// that has already re-checked; these make either order settle on the
/// user's intent, which is to keep the storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PodEnd {
    /// The model's `terminate_pod`, a lost pod, a removal: as
    /// `terminate_sandbox_pod`.
    Plain,
    /// A quiet stop: closes only a row still live, stamping
    /// `quietly_stopped_at` in the same statement. `None` for a row
    /// already ended, which the quiet stop reads as "already stopped".
    Quiet,
    /// The user's Stop: closes the row (keeping an earlier end's time)
    /// and clears any quiet stop's mark, so the storage-grace pass never
    /// removes storage the Stop keeps.
    ByUser,
}

/// `terminate_sandbox_pod`, closing the row the way `end` says.
pub async fn terminate_sandbox_pod_as(
    pool: &PgPool,
    pod_id: i64,
    end: PodEnd,
) -> Result<Option<SandboxPod>, sqlx::Error> {
    let sql = match end {
        PodEnd::Plain => "UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1 RETURNING *",
        PodEnd::Quiet => {
            "UPDATE sandbox_pods SET terminated_at = now(), quietly_stopped_at = now()
              WHERE id = $1 AND terminated_at IS NULL RETURNING *"
        }
        PodEnd::ByUser => {
            "UPDATE sandbox_pods SET terminated_at = COALESCE(terminated_at, now()), quietly_stopped_at = NULL
              WHERE id = $1 RETURNING *"
        }
    };
    sqlx::query_as::<_, SandboxPod>(sql).bind(pod_id).fetch_optional(pool).await
}

/// One live pod, across every conversation, with what the pods view needs
/// from the database: its conversation, and its terminals' activity.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct LivePodRow {
    pub pod_id: i64,
    pub conversation_id: i64,
    pub conversation_title: String,
    pub created_at: NaiveDateTime,
    pub live_terminals: i64,
    pub running_commands: i64,
    /// The latest sign of activity the database holds (`POD_IDLE_SINCE`):
    /// the pod starting, a flushed preview, dial or turn end, a command
    /// starting or finishing (SME-78).
    pub idle_since: NaiveDateTime,
    /// A command running or a repo cloning (`POD_BUSY`): never stopped
    /// while true. A running turn is in memory, checked by the caller.
    pub busy: bool,
    /// The database's own `now()`, so ages are worked out on one clock.
    pub observed_at: NaiveDateTime,
}

/// Every live pod's id, oldest first: the startup reconnect's list
/// (SME-359).
pub async fn live_sandbox_pod_ids(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM sandbox_pods WHERE terminated_at IS NULL ORDER BY id")
        .fetch_all(pool)
        .await
}

/// Every live pod, oldest first — the pods view's rows.
pub async fn list_live_pods(pool: &PgPool) -> Result<Vec<LivePodRow>, sqlx::Error> {
    sqlx::query_as::<_, LivePodRow>(&format!(
        "SELECT p.id AS pod_id,
                p.conversation_id,
                c.title AS conversation_title,
                p.created_at,
                (SELECT COUNT(*) FROM sandbox_terminals t
                  WHERE t.pod_id = p.id AND t.terminated_at IS NULL) AS live_terminals,
                (SELECT COUNT(*) FROM terminal_commands tc
                   JOIN sandbox_terminals t ON t.id = tc.terminal_id
                  WHERE t.pod_id = p.id AND tc.status = 'running') AS running_commands,
                {POD_IDLE_SINCE} AS idle_since,
                {POD_BUSY} AS busy,
                now()::timestamp AS observed_at
           FROM sandbox_pods p
           JOIN conversations c ON c.id = p.conversation_id
          WHERE p.terminated_at IS NULL
          ORDER BY p.created_at ASC, p.id ASC"
    ))
    .fetch_all(pool)
    .await
}

// --- Quiet stops (SME-78) ---
//
// One rule, in three fragments shared by `/pods`, the idle pass's
// candidates and the re-check under the locks, so the three can't
// disagree. `sandbox_pods` must be aliased `p`.

/// The latest sign of activity the database holds for pod `p`: its start,
/// its last flushed note (`last_active_at`), and its commands' last start
/// and last finish. `GREATEST` skips NULLs. A message is not in it: SME-79's
/// idle notices are messages (the user's answer 2).
const POD_IDLE_SINCE: &str = "GREATEST(p.created_at, p.last_active_at, \
        (SELECT MAX(GREATEST(tc.created_at, tc.finished_at)) FROM terminal_commands tc \
           JOIN sandbox_terminals t ON t.id = tc.terminal_id WHERE t.pod_id = p.id))";

/// Pod `p` is busy, so never stopped: a repo cloning in its conversation,
/// or any command running in its terminals, whatever its idle-notice
/// setting (the user's answer 1). A running turn is in memory and checked
/// in Rust.
const POD_BUSY: &str = "(EXISTS (SELECT 1 FROM conversation_repos r \
        WHERE r.conversation_id = p.conversation_id AND r.status = 'cloning') \
     OR EXISTS (SELECT 1 FROM terminal_commands tc \
        JOIN sandbox_terminals t ON t.id = tc.terminal_id \
        WHERE t.pod_id = p.id AND tc.status = 'running'))";

/// How often the idle pass (`sandbox::stop_idle_pods`) runs, in seconds.
/// The pass's tick and `FINISHED_GRACE_SECS` are both derived from it.
pub const QUIET_STOP_PASS_SECS: u64 = 60;

/// How long after an agent's finish a flushed note (`last_active_at`)
/// still counts as part of the finish: the finish turn's own end is noted
/// after `state_changed_at`, and reaches the database at the next pass's
/// flush, up to one pass later. Two passes covers both (SME-78 review 1, A2).
const FINISHED_GRACE_SECS: u64 = 2 * QUIET_STOP_PASS_SECS;

/// Pod `p` is a `finished` agent's and nothing used it after the finish
/// (SME-78 review 1, A2): it was started at or before the finish, no
/// command started in it since, and its last note is no later than
/// `FINISHED_GRACE_SECS` after the later of the finish and the last end of
/// a command that started before it (review 2, R2-1: that command's exit
/// wake notes activity, flushed after its end). A command left running
/// that ends after the finish doesn't count, so the backstop stops the pod
/// once it does. A pod the user started or used after the finish (Work on a repo,
/// a preview) falls back to the idle rule. The one copy of the rule, for
/// the candidates and the re-check alike.
fn pod_finished() -> String {
    format!(
        "EXISTS (SELECT 1 FROM agents a \
           WHERE a.conversation_id = p.conversation_id AND a.state = 'finished' \
             AND p.created_at <= a.state_changed_at \
             AND (p.last_active_at IS NULL \
                  OR p.last_active_at <= GREATEST(a.state_changed_at, \
                         (SELECT MAX(tc.finished_at) FROM terminal_commands tc \
                            JOIN sandbox_terminals t ON t.id = tc.terminal_id \
                           WHERE t.pod_id = p.id AND tc.created_at <= a.state_changed_at)) \
                     + make_interval(secs => {FINISHED_GRACE_SECS})) \
             AND NOT EXISTS (SELECT 1 FROM terminal_commands tc \
                  JOIN sandbox_terminals t ON t.id = tc.terminal_id \
                  WHERE t.pod_id = p.id AND tc.created_at > a.state_changed_at))"
    )
}

/// Pod `p` has been idle for `$1` minutes or more; never when `$1` is 0.
const POD_IDLE_PAST: &str = "($1 > 0 AND now() - {POD_IDLE_SINCE} >= make_interval(mins => $1))";

fn pod_idle_past() -> String {
    POD_IDLE_PAST.replace("{POD_IDLE_SINCE}", POD_IDLE_SINCE)
}

/// Why a pod is to be stopped quietly: idle past the threshold, or its
/// conversation is a `finished` agent's (at any age).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuietStopKind {
    Idle,
    Finished,
}

/// Live pods that aren't busy and are either idle for `minutes` or more
/// (none when `minutes` is 0) or a `finished` agent's, as
/// `(pod_id, conversation_id, kind)`, oldest first. A finished agent's pod
/// is `Finished` whatever its age.
pub async fn quiet_stop_candidates(
    pool: &PgPool,
    minutes: u32,
) -> Result<Vec<(i64, i64, QuietStopKind)>, sqlx::Error> {
    let rows: Vec<(i64, i64, bool)> = sqlx::query_as(&format!(
        "SELECT p.id, p.conversation_id, {finished} AS finished
           FROM sandbox_pods p
          WHERE p.terminated_at IS NULL AND NOT {POD_BUSY}
            AND ({finished} OR {idle_past})
          ORDER BY p.created_at ASC, p.id ASC",
        finished = pod_finished(),
        idle_past = pod_idle_past(),
    ))
    .bind(minutes_param(minutes))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(pod, conversation, finished)| {
            (pod, conversation, if finished { QuietStopKind::Finished } else { QuietStopKind::Idle })
        })
        .collect())
}

/// `minutes` as the `INT` `make_interval(mins => …)` takes; the parser's
/// week ceiling keeps it far inside the range.
fn minutes_param(minutes: u32) -> i32 {
    i32::try_from(minutes).unwrap_or(i32::MAX)
}

/// The re-check under the turn and start locks, in one statement: `pod_id`
/// is live, not busy, and still stoppable for `kind` (idle past `minutes`,
/// or its agent still `finished`).
pub async fn pod_still_stoppable(
    pool: &PgPool,
    pod_id: i64,
    kind: QuietStopKind,
    minutes: u32,
) -> Result<bool, sqlx::Error> {
    let rule = match kind {
        QuietStopKind::Idle => pod_idle_past(),
        QuietStopKind::Finished => pod_finished(),
    };
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM sandbox_pods p
          WHERE p.id = $2 AND p.terminated_at IS NULL AND NOT {POD_BUSY} AND {rule})"
    ))
    .bind(minutes_param(minutes))
    .bind(pod_id)
    .fetch_one(pool)
    .await
}

/// Stamps `last_active_at` with the database's `now()` on the live pods of
/// `conversation_ids` — `note_activity`'s flush. Terminated pods are left
/// alone.
pub async fn flush_pod_activity(pool: &PgPool, conversation_ids: &[i64]) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE sandbox_pods SET last_active_at = now()
          WHERE conversation_id = ANY($1) AND terminated_at IS NULL",
    )
    .bind(conversation_ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// Live pods created more than `min_age_secs` seconds ago, by the
/// database's clock — the ones old enough to have a pod in Kubernetes.
pub async fn live_pods_older_than(pool: &PgPool, min_age_secs: i64) -> Result<Vec<SandboxPod>, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "SELECT * FROM sandbox_pods
          WHERE terminated_at IS NULL AND created_at < now() - make_interval(secs => $1)
          ORDER BY id ASC",
    )
    .bind(min_age_secs as f64)
    .fetch_all(pool)
    .await
}

/// Whether `pod_id` exists and hasn't been terminated.
pub async fn sandbox_pod_is_live(pool: &PgPool, pod_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sandbox_pods WHERE id = $1 AND terminated_at IS NULL)",
    )
    .bind(pod_id)
    .fetch_one(pool)
    .await
}

/// The conversations that have a live pod — the sidebar's markers.
pub async fn conversations_with_live_pods(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT conversation_id FROM sandbox_pods
          WHERE terminated_at IS NULL ORDER BY conversation_id ASC",
    )
    .fetch_all(pool)
    .await
}

/// The conversations with at least one running terminal command on a
/// live pod, with the count — the sidebar's markers (SME-441). Same
/// predicate as `list_live_pods`' `running_commands` and `POD_BUSY`.
pub async fn running_command_conversation_counts(
    pool: &PgPool,
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT p.conversation_id, COUNT(*)::BIGINT
           FROM sandbox_pods p
           JOIN sandbox_terminals t ON t.pod_id = p.id
           JOIN terminal_commands tc ON tc.terminal_id = t.id
          WHERE p.terminated_at IS NULL AND tc.status = 'running'
          GROUP BY p.conversation_id
          ORDER BY p.conversation_id ASC",
    )
    .fetch_all(pool)
    .await
}

/// `running_command_conversation_counts` for one conversation (SME-516):
/// what a persona refresh that would restart the pod checks first.
pub async fn running_command_count(pool: &PgPool, conversation_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
           FROM sandbox_pods p
           JOIN sandbox_terminals t ON t.pod_id = p.id
           JOIN terminal_commands tc ON tc.terminal_id = t.id
          WHERE p.conversation_id = $1 AND p.terminated_at IS NULL AND tc.status = 'running'",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
}

/// Live pods only (`terminated_at IS NULL`) — a terminated pod's row
/// sticks around (see SME-9's "How") but shouldn't be listed as if it
/// still existed.
pub async fn list_sandbox_pods(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<SandboxPod>, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "SELECT * FROM sandbox_pods WHERE conversation_id = $1 AND terminated_at IS NULL ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

pub async fn create_sandbox_terminal(
    pool: &PgPool,
    pod_id: i64,
) -> Result<SandboxTerminal, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "INSERT INTO sandbox_terminals (pod_id) VALUES ($1) RETURNING *",
    )
    .bind(pod_id)
    .fetch_one(pool)
    .await
}

/// Same idempotent-on-repeat, real-error-on-unknown-id shape as
/// `terminate_sandbox_pod`.
pub async fn terminate_sandbox_terminal(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<SandboxTerminal>, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "UPDATE sandbox_terminals SET terminated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(terminal_id)
    .fetch_optional(pool)
    .await
}

/// Live terminals for one pod — backs `terminate_pod`'s guard (refuse if
/// non-empty).
pub async fn list_sandbox_terminals_for_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Vec<SandboxTerminal>, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "SELECT * FROM sandbox_terminals WHERE pod_id = $1 AND terminated_at IS NULL ORDER BY id ASC",
    )
    .bind(pod_id)
    .fetch_all(pool)
    .await
}

/// Live terminals across every pod in a conversation — backs
/// `list_terminals`, which shows the model everything it has regardless
/// of which pod it's in.
pub async fn list_sandbox_terminals_for_conversation(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<SandboxTerminal>, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "SELECT st.* FROM sandbox_terminals st
         JOIN sandbox_pods sp ON sp.id = st.pod_id
         WHERE sp.conversation_id = $1 AND st.terminated_at IS NULL
         ORDER BY st.id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// The owning pod for a terminal — `sandbox.rs` needs this before it can
/// look up (or establish) that pod's agent connection.
pub async fn sandbox_terminal_pod_id(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT pod_id FROM sandbox_terminals WHERE id = $1")
        .bind(terminal_id)
        .fetch_optional(pool)
        .await
}

/// The conversation whose pod `terminal_id` is in, if it exists: the tools
/// only act on the calling conversation's terminals (SME-51 B2).
pub async fn terminal_conversation_id(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT p.conversation_id FROM sandbox_terminals t
           JOIN sandbox_pods p ON p.id = t.pod_id
          WHERE t.id = $1",
    )
    .bind(terminal_id)
    .fetch_optional(pool)
    .await
}

/// SME-425: the terminal's pod, whether its terminal row is closed, and
/// whether that pod's row has ended — one indexed read for the up-front
/// stale-terminal refusal (`sandbox::refuse_stale_terminal`), which must
/// decide before anything is dialled or written. `None` for an unknown
/// terminal id; the join can't miss, as a terminal's pod row is never
/// deleted.
pub async fn terminal_pod_state(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<(i64, bool, bool)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, bool, bool)>(
        "SELECT t.pod_id, t.terminated_at IS NOT NULL, p.terminated_at IS NOT NULL \
           FROM sandbox_terminals t JOIN sandbox_pods p ON p.id = t.pod_id \
          WHERE t.id = $1",
    )
    .bind(terminal_id)
    .fetch_optional(pool)
    .await
}

/// Resolves which conversation's `events::publish` bus a pod-scoped call
/// (`terminate_pod`, `create_terminal`, `terminate_terminal`, crash
/// cleanup) should target — see
/// SME-10.
pub async fn sandbox_pod_conversation_id(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT conversation_id FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_optional(pool)
        .await
}

/// Records `host:port` as a preview the model shared for `pod_id` (SME-42)
/// and returns every preview shared for that pod so far, as
/// `list_pod_previews` orders them. `host` is `""` for the pod's own
/// localhost, or a Docker container's address (SME-33). Sharing one twice
/// is fine.
pub async fn add_pod_preview(
    pool: &PgPool,
    pod_id: i64,
    host: &str,
    port: u16,
) -> Result<Vec<(String, u16)>, sqlx::Error> {
    sqlx::query("INSERT INTO sandbox_pod_previews (pod_id, host, port) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(pod_id)
        .bind(host)
        .bind(i32::from(port))
        .execute(pool)
        .await?;
    list_pod_previews(pool, pod_id).await
}

/// Every preview shared for `pod_id` as `(host, port)`: the pod's own
/// localhost (`""`) first, then by container address, each lowest port first.
pub async fn list_pod_previews(pool: &PgPool, pod_id: i64) -> Result<Vec<(String, u16)>, sqlx::Error> {
    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT host, port FROM sandbox_pod_previews WHERE pod_id = $1 ORDER BY host, port",
    )
    .bind(pod_id)
    .fetch_all(pool)
    .await?;
    // The table's CHECK keeps every port in u16's range.
    Ok(rows
        .into_iter()
        .filter_map(|(host, port)| Some((host, u16::try_from(port).ok()?)))
        .collect())
}

