use super::*;

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
    sqlx::query_as::<_, SandboxPod>(
        "UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(pod_id)
    .fetch_optional(pool)
    .await
}

/// One live pod, across every conversation, with what the pods view needs
/// from the database: its conversation, and its terminals' activity.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct LivePodRow {
    pub pod_id: i64,
    pub conversation_id: i64,
    pub conversation_title: String,
    /// Bumped by every message, so it stands in for tool activity that
    /// leaves no per-pod record.
    pub conversation_updated_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
    pub live_terminals: i64,
    pub running_commands: i64,
    pub last_command_finished_at: Option<NaiveDateTime>,
    /// The database's own `now()`, so ages are worked out on one clock.
    pub observed_at: NaiveDateTime,
}

/// Every live pod, oldest first — the pods view's rows.
pub async fn list_live_pods(pool: &PgPool) -> Result<Vec<LivePodRow>, sqlx::Error> {
    sqlx::query_as::<_, LivePodRow>(
        "SELECT p.id AS pod_id,
                p.conversation_id,
                c.title AS conversation_title,
                c.updated_at AS conversation_updated_at,
                p.created_at,
                (SELECT COUNT(*) FROM sandbox_terminals t
                  WHERE t.pod_id = p.id AND t.terminated_at IS NULL) AS live_terminals,
                (SELECT COUNT(*) FROM terminal_commands tc
                   JOIN sandbox_terminals t ON t.id = tc.terminal_id
                  WHERE t.pod_id = p.id AND tc.status = 'running') AS running_commands,
                (SELECT MAX(tc.finished_at) FROM terminal_commands tc
                   JOIN sandbox_terminals t ON t.id = tc.terminal_id
                  WHERE t.pod_id = p.id) AS last_command_finished_at,
                now()::timestamp AS observed_at
           FROM sandbox_pods p
           JOIN conversations c ON c.id = p.conversation_id
          WHERE p.terminated_at IS NULL
          ORDER BY p.created_at ASC, p.id ASC",
    )
    .fetch_all(pool)
    .await
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct TerminalCommand {
    pub id: i64,
    pub conversation_id: i64,
    pub terminal_id: i64,
    pub command_id: String,
    pub command: String,
    /// "running" | "finished" | "lost" — enforced by the table's CHECK
    /// constraint, not re-validated here.
    pub status: String,
    pub exit_code: Option<i32>,
    pub notified_at: Option<NaiveDateTime>,
    /// SME-79: this command's idle-notice interval — how long it may go
    /// untouched before smelt tells the model it's still running. Stored
    /// when the command starts (the deployment default
    /// `SANDBOX_IDLE_NOTICE_MINUTES`, or the `idle_notice_seconds`
    /// `run_terminal_command` was given), so it keeps that interval if the
    /// default changes later. `0` means never: this command gets no notice.
    pub idle_notice_secs: i32,
    /// The last moment the model touched the command — checked its status,
    /// read its output, sent it a signal, re-timed its notice; `None` when
    /// it never has, so the idle clock counts from `created_at`. Output on
    /// its own does not move it (SME-79).
    pub touched_at: Option<NaiveDateTime>,
    /// When smelt last sent this command's idle notice, `None` until the
    /// first one. It is part of the clock, so each notice restarts it and a
    /// command that is never touched keeps being noticed (SME-79).
    pub idle_notice_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub finished_at: Option<NaiveDateTime>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct TerminalCommandStatus {
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout_lines: i64,
    pub stderr_lines: i64,
    /// SME-79: the idle-notice interval in force for this command, `0`
    /// when it's turned off.
    pub idle_notice_secs: i32,
    /// Whole seconds since the later of the command's start and its last
    /// touch — the same clock the idle notice fires on. `None` once it
    /// isn't running: a finished command can't go untouched (SME-79).
    pub untouched_for_seconds: Option<i64>,
}

/// A running command whose idle notice is due, with everything the notice's
/// sentence needs (SME-79): the command line, the output counts, and the two
/// durations. All of it is read at this moment, and the durations are
/// computed by the database, so the sentence is never stale and never moves
/// with the app's clock.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct IdleCommand {
    pub command_id: String,
    pub command: String,
    pub stdout_lines: i64,
    pub stderr_lines: i64,
    pub running_for_secs: i64,
    pub untouched_for_secs: i64,
}

/// Whole seconds from the later of the command's start and its last touch,
/// computed by the database and rounded down — a sentence never overstates
/// how long a command has been left alone. `terminal_commands` must be
/// aliased `tc` (SME-79).
const UNTOUCHED_FOR_SECS: &str = "FLOOR(EXTRACT(EPOCH FROM now() - GREATEST(\
        tc.created_at, COALESCE(tc.touched_at, tc.created_at))))::BIGINT";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct TerminalLine {
    pub stream: String,
    pub data: String,
    /// Global order across *both* streams for one command — a caller that
    /// fetches stdout and stderr as two separate calls (e.g. to cap each
    /// stream's tail independently, see `api::sandbox::fetch_command_summary`)
    /// needs this to merge them back into the order they actually
    /// happened in, rather than showing "all stdout, then all stderr."
    pub seq: i64,
}

/// Starts the row for a command that is about to run. `idle_notice_secs` is
/// the interval this command will be watched on — the deployment default, or
/// what `run_terminal_command` was given, `0` for never — and it is stored
/// here, at the start, so the row says what it actually uses even if the
/// deployment's default changes while the command runs (SME-79).
pub async fn create_terminal_command(
    pool: &PgPool,
    conversation_id: i64,
    terminal_id: i64,
    command_id: &str,
    command: &str,
    idle_notice_secs: i32,
) -> Result<TerminalCommand, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "INSERT INTO terminal_commands (conversation_id, terminal_id, command_id, command, status, idle_notice_secs)
         VALUES ($1, $2, $3, $4, 'running', $5) RETURNING *",
    )
    .bind(conversation_id)
    .bind(terminal_id)
    .bind(command_id)
    .bind(command)
    .bind(idle_notice_secs)
    .fetch_one(pool)
    .await
}

/// Backs the single-command-in-flight check `run_terminal_command` and
/// `terminate_terminal` both need — at most one row per terminal can
/// ever be `running` at a time (one in-flight command per terminal, not
/// per conversation — see SME-9's "What"), enforced at the tool layer,
/// not by a database constraint.
pub async fn terminal_command_is_running(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "SELECT * FROM terminal_commands WHERE terminal_id = $1 AND status = 'running'",
    )
    .bind(terminal_id)
    .fetch_optional(pool)
    .await
}

pub async fn append_terminal_event(
    pool: &PgPool,
    command_id: &str,
    stream: &str,
    seq: i64,
    data: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO terminal_events (command_id, stream, seq, data) VALUES ($1, $2, $3, $4)",
    )
    .bind(command_id)
    .bind(stream)
    .bind(seq)
    .bind(data)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_terminal_command_finished(
    pool: &PgPool,
    command_id: &str,
    exit_code: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE terminal_commands SET status = 'finished', exit_code = $2, finished_at = now()
         WHERE command_id = $1",
    )
    .bind(command_id)
    .bind(exit_code)
    .execute(pool)
    .await?;
    Ok(())
}

/// Used only by the crash-recovery path: a command that was `running` when
/// its agent was found unreachable has no real exit code to report, so
/// `exit_code` stays `NULL` — see SME-9's "Agent crash recovery."
/// Restricted to rows still `running` so this is safe to call defensively
/// without first checking status.
pub async fn mark_terminal_command_lost(
    pool: &PgPool,
    command_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE terminal_commands SET status = 'lost', finished_at = now()
         WHERE command_id = $1 AND status = 'running'",
    )
    .bind(command_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `send_signal` addresses a command purely by its (globally-unique)
/// `command_id`, same as `terminal_command_status`/`read_terminal_output`
/// — but `sandbox::send_signal` still needs `terminal_id` to know which
/// pod to reach, hence this lookup.
pub async fn get_terminal_command(
    pool: &PgPool,
    command_id: &str,
) -> Result<Option<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>("SELECT * FROM terminal_commands WHERE command_id = $1")
        .bind(command_id)
        .fetch_optional(pool)
        .await
}

/// A snapshot query, not a wait — `terminal_command_status` never blocks.
/// Line counts come from `terminal_events` via a `LEFT JOIN` so a command
/// with zero output of a given stream still returns `0`, not `NULL`. The
/// idle-notice fields come along too (SME-79): the interval in force, and
/// how long the command has gone untouched — but only while it's running,
/// since a finished command can't be waiting on anyone.
pub async fn terminal_command_status(
    pool: &PgPool,
    command_id: &str,
) -> Result<Option<TerminalCommandStatus>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommandStatus>(&format!(
        "SELECT tc.status, tc.exit_code,
                COALESCE(SUM(CASE WHEN te.stream = 'stdout' THEN 1 ELSE 0 END), 0) AS stdout_lines,
                COALESCE(SUM(CASE WHEN te.stream = 'stderr' THEN 1 ELSE 0 END), 0) AS stderr_lines,
                tc.idle_notice_secs,
                CASE WHEN tc.status = 'running' THEN {UNTOUCHED_FOR_SECS} END AS untouched_for_seconds
         FROM terminal_commands tc
         LEFT JOIN terminal_events te ON te.command_id = tc.command_id
         WHERE tc.command_id = $1
         GROUP BY tc.id",
    ))
    .bind(command_id)
    .fetch_optional(pool)
    .await
}

/// `LIMIT`/`OFFSET` over the requested stream(s), ordered by the agent-
/// assigned `seq` — *not* `id` (see SME-9's "Ordering" section on why
/// insertion order isn't trusted as the real order). `streams` is typically
/// `&["stdout"]`, `&["stderr"]`, or `&["stdout", "stderr"]` — line numbers
/// are relative to whichever set is requested, by design.
pub async fn read_terminal_output(
    pool: &PgPool,
    command_id: &str,
    streams: &[&str],
    offset: i64,
    limit: i64,
) -> Result<Vec<TerminalLine>, sqlx::Error> {
    sqlx::query_as::<_, TerminalLine>(
        "SELECT stream, data, seq FROM terminal_events
         WHERE command_id = $1 AND stream = ANY($2)
         ORDER BY seq ASC
         OFFSET $3 LIMIT $4",
    )
    .bind(command_id)
    .bind(streams)
    .bind(offset)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Backs the completion-notification check in `run_turn`'s loop — both
/// terminal states (`finished` and `lost`) need exactly one notification
/// each, so this matches either, not just `finished`.
pub async fn unnotified_finished_terminal_commands(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "SELECT * FROM terminal_commands
         WHERE conversation_id = $1 AND status IN ('finished', 'lost') AND notified_at IS NULL
         ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// Only tests mark a command without its notice; a turn uses
/// `save_command_notice`, which does both at once.
#[cfg(test)]
pub async fn mark_terminal_command_notified(
    pool: &PgPool,
    command_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE terminal_commands SET notified_at = now() WHERE command_id = $1")
        .bind(command_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Saves `content` as `command_id`'s notice in `conversation_id` and marks
/// the command notified, in one transaction: a turn stopped part-way
/// leaves both or neither, so the next drain can't save the notice again
/// (SME-91). `None`, with nothing saved, when the command was already
/// notified or doesn't exist.
pub async fn save_command_notice(
    pool: &PgPool,
    conversation_id: i64,
    command_id: &str,
    content: &[ContentBlock],
) -> Result<Option<Message>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let marked = sqlx::query(
        "UPDATE terminal_commands SET notified_at = now()
         WHERE command_id = $1 AND notified_at IS NULL",
    )
    .bind(command_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if marked == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    let saved = create_message_on(&mut tx, conversation_id, "user", content).await?;
    tx.commit().await?;
    Ok(Some(saved))
}

// --- Idle notices (SME-79) ---

/// The idle-notice rule, in one fragment shared by the watch's pass, the
/// turn's drain and the guard on saving the notice, so the three can't
/// disagree: a running command with an interval, whose clock — the latest of
/// its start, its last touch and its last notice — is at least that interval
/// old. Output is not in it, and neither is the app's clock: every time is
/// compared in SQL, so no zone or skew can move a deadline. Each notice
/// restarts the clock through `idle_notice_at`, so a command nobody touches
/// is noticed again every interval, without limit; touching it restarts the
/// clock through `touched_at` instead. `terminal_commands` must be aliased
/// `tc`, since the drain joins `terminal_events`, which has its own
/// `created_at`.
const IDLE_NOTICE_DUE: &str = "tc.status = 'running' AND tc.idle_notice_secs > 0 \
    AND now() - GREATEST(tc.created_at, COALESCE(tc.touched_at, tc.created_at), COALESCE(tc.idle_notice_at, tc.created_at)) \
        >= make_interval(secs => tc.idle_notice_secs)";

/// Every running command whose idle notice is due, oldest first, as
/// `(conversation_id, command_id)` — the watch's per-pass check. It decides
/// only *which* conversations to wake: the wake's drain (`idle_commands_due`)
/// re-reads the same fragment under the conversation's turn lock before
/// saving anything, so this list being a moment stale is harmless.
pub async fn idle_commands_to_notice(pool: &PgPool) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String)>(&format!(
        "SELECT tc.conversation_id, tc.command_id FROM terminal_commands tc \
         WHERE {IDLE_NOTICE_DUE} ORDER BY tc.conversation_id ASC, tc.id ASC"
    ))
    .fetch_all(pool)
    .await
}

/// `idle_commands_to_notice` scoped to one conversation, with everything the
/// notice sentence needs — the drain's side of SME-79, read under the
/// conversation's turn lock. The counts and durations are this query's, so
/// what the model reads is true at the moment it's told.
pub async fn idle_commands_due(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<IdleCommand>, sqlx::Error> {
    sqlx::query_as::<_, IdleCommand>(&format!(
        "SELECT tc.command_id, tc.command,
                COALESCE(SUM(CASE WHEN te.stream = 'stdout' THEN 1 ELSE 0 END), 0) AS stdout_lines,
                COALESCE(SUM(CASE WHEN te.stream = 'stderr' THEN 1 ELSE 0 END), 0) AS stderr_lines,
                FLOOR(EXTRACT(EPOCH FROM now() - tc.created_at))::BIGINT AS running_for_secs,
                {UNTOUCHED_FOR_SECS} AS untouched_for_secs
         FROM terminal_commands tc
         LEFT JOIN terminal_events te ON te.command_id = tc.command_id
         WHERE tc.conversation_id = $1 AND {IDLE_NOTICE_DUE}
         GROUP BY tc.id
         ORDER BY tc.id ASC"
    ))
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// The model touched `command_id` — checked its status, read its output,
/// sent it a signal, re-timed its notice — so its idle clock restarts from
/// now (SME-79). Only while it's running: a finished command has no clock
/// left to move, and its finish notice is already on its way.
pub async fn touch_terminal_command(pool: &PgPool, command_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE terminal_commands SET touched_at = now() WHERE command_id = $1 AND status = 'running'",
    )
    .bind(command_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Re-times a running command — the new interval (SME-79), and a clock
/// restarted from this call, so the new interval counts from when the model
/// asked rather than from wherever the old clock was running. `None` when
/// the command isn't running: idle notices are only for commands still going.
pub async fn set_terminal_command_notice_secs(
    pool: &PgPool,
    command_id: &str,
    secs: i32,
) -> Result<Option<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "UPDATE terminal_commands SET idle_notice_secs = $2, touched_at = now() \
         WHERE command_id = $1 AND status = 'running' RETURNING *",
    )
    .bind(command_id)
    .bind(secs)
    .fetch_optional(pool)
    .await
}

/// Saves `content` as `command_id`'s idle notice in `conversation_id` and
/// marks that notice sent, in one transaction — SME-91's discipline, in the
/// idle shape of `save_command_notice`: the guard re-checks the whole due
/// rule, so a command that finished, or was touched, or whose notice another
/// drain already saved, gets nothing. `idle_notice_at` is this notice's mark;
/// `notified_at` is left alone, since the finish notice is still owed after
/// an idle one.
pub async fn save_idle_notice(
    pool: &PgPool,
    conversation_id: i64,
    command_id: &str,
    content: &[ContentBlock],
) -> Result<Option<Message>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let marked = sqlx::query(&format!(
        "UPDATE terminal_commands tc SET idle_notice_at = now() \
         WHERE tc.command_id = $1 AND {IDLE_NOTICE_DUE} RETURNING tc.id"
    ))
    .bind(command_id)
    .fetch_optional(&mut *tx)
    .await?;
    if marked.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }
    let saved = create_message_on(&mut tx, conversation_id, "user", content).await?;
    tx.commit().await?;
    Ok(Some(saved))
}

/// Backs `list_commands` — most-recent-first, bounded the same way
/// `read_terminal_output` is, scoped to one terminal (its history
/// outlives that terminal being torn down, since `terminal_commands`
/// isn't cascade-deleted by `terminate_terminal` — see SME-9's "How").
pub async fn list_terminal_commands(
    pool: &PgPool,
    terminal_id: i64,
    limit: i64,
) -> Result<Vec<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "SELECT * FROM terminal_commands WHERE terminal_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(terminal_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}
