//! Terminals and their commands, and the file tools, all through the
//! sandbox agent.

use super::*;

// --- Terminal ---

/// Takes `conversation_id`, resolved to "the conversation's pod" via
/// `conversation_pod_id` — errors `NoPod` if none exists yet (create_pod
/// first), same requirement as before, just checked against the DB instead
/// of trusting a caller-supplied `pod_id`. Every call creates a genuinely
/// new terminal in that pod — no idempotency to preserve with N terminals
/// per pod. Establishes the pod's agent connection if it isn't already
/// live in this process's registry — the agent itself is already running
/// by the time the pod is `Running` (it's the pod's own `ENTRYPOINT`, see
/// `ensure_pod_connection`), so this is just "connect," never "launch."
pub async fn create_terminal(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = ensure_pod_connection(pool, pod_id).await?;

    let row = db::create_sandbox_terminal(pool, pod_id).await?;
    let terminal_id = row.id;

    let created = conn
        .request(|request_id| ClientMessage::CreateTerminal { request_id, terminal_id: terminal_id.to_string() })
        .await;
    if let Err(e) = expect_reply(created, "create_terminal", |reply| {
        matches!(reply, Reply::TerminalCreated).then_some(())
    }) {
        let _ = db::terminate_sandbox_terminal(pool, terminal_id).await;
        return Err(e);
    }
    events::publish(
        conn.conversation_id,
        events::ConversationEvent::SandboxTerminalUpdate {
            pod_id,
            terminal_id,
            status: "connected".to_string(),
            terminated: false,
        },
    );
    Ok(terminal_id)
}

/// Idempotent on repeat (see SME-9's "How"). Refuses if a command is
/// still `running` in this terminal — the model must `send_signal`/wait
/// it out first. Otherwise asks the agent to `killpg` just this
/// terminal's shell — see SME-9's "Terminating a terminal without
/// touching the pod, or its siblings."
pub async fn terminate_terminal(pool: &PgPool, terminal_id: i64) -> Result<(), TerminalError> {
    let Some(pod_id) = db::sandbox_terminal_pod_id(pool, terminal_id).await? else {
        return Err(TerminalError::NoTerminal);
    };

    let live = db::list_sandbox_terminals_for_pod(pool, pod_id).await?;
    if !live.iter().any(|t| t.id == terminal_id) {
        return Ok(()); // already terminated (or crash-cleanup already cleared it) — idempotent
    }

    if let Ok(Some(_)) = db::terminal_command_is_running(pool, terminal_id).await {
        return Err(TerminalError::CommandStillRunning);
    }

    let conn = reconnect_if_needed(pool, pod_id).await?;
    let terminated = conn
        .request(|request_id| ClientMessage::TerminateTerminal { request_id, terminal_id: terminal_id.to_string() })
        .await;
    expect_reply(terminated, "terminate_terminal", |reply| {
        matches!(reply, Reply::TerminalTerminated).then_some(())
    })?;

    db::terminate_sandbox_terminal(pool, terminal_id).await?;
    events::publish(
        conn.conversation_id,
        events::ConversationEvent::SandboxTerminalUpdate {
            pod_id,
            terminal_id,
            status: "disconnected".to_string(),
            terminated: true,
        },
    );
    Ok(())
}

/// Every live terminal across every pod in the conversation, not just one
/// pod's — the model can always ask what it has without tracking pod_ids
/// itself. `status` reflects whether the *owning pod's* connection is
/// currently live, not anything about the terminal individually (there's
/// nothing per-terminal to check — one connection serves a whole pod).
pub async fn list_terminals(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<TerminalInfo>, SandboxError> {
    let rows = db::list_sandbox_terminals_for_conversation(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    Ok(rows
        .into_iter()
        .map(|t| TerminalInfo {
            terminal_id: t.id,
            pod_id: t.pod_id,
            status: if registry_contains(t.pod_id) {
                "connected"
            } else {
                "disconnected"
            }
            .to_string(),
        })
        .collect())
}

/// Sends a `command` to the terminal's pod's agent — reconnecting first if the
/// registry has no live entry (a smelt restart, or the first send right
/// after `create_terminal`'s own connect). Never launches a fresh agent
/// itself (that's `create_terminal`'s job).
pub async fn send_command(
    pool: &PgPool,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> Result<(), TerminalError> {
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id)
        .await?
        .ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    Ok(conn.send(&ClientMessage::Command {
        terminal_id: terminal_id.to_string(),
        id: command_id.to_string(),
        command: command.to_string(),
    })?)
}

/// Why `run_command` didn't start a command.
#[derive(Debug)]
pub enum RunCommandError {
    /// The terminal is still running this command; one at a time.
    Busy(db::TerminalCommand),
    Failed(String),
}

/// Starts `command` in `terminal_id` as `command_id`: refuses while another
/// command runs there, records it as running, publishes it to the sandbox
/// panel as started, and sends it to the pod's agent. A send that fails
/// marks the row lost, and says so to the panel, so nothing waits on a
/// command that never ran.
pub async fn run_command(
    pool: &PgPool,
    conversation_id: i64,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> Result<(), RunCommandError> {
    if let Some(running) = db::terminal_command_is_running(pool, terminal_id)
        .await
        .map_err(|e| RunCommandError::Failed(e.to_string()))?
    {
        return Err(RunCommandError::Busy(running));
    }

    db::create_terminal_command(pool, conversation_id, terminal_id, command_id, command)
        .await
        .map_err(|e| RunCommandError::Failed(e.to_string()))?;

    // Announced before the send (SME-144): the agent's output and exit
    // come back on the pod's reader task, and an announcement made after
    // the send could reach open tabs after a fast command's finish, which
    // a tab would then apply to the previous command. Published
    // immediately, before any output, so the panel shows it started
    // (SME-10).
    publish_command_update(conversation_id, terminal_id, command_id, Some(command), "running");

    if let Err(e) = send_command(pool, terminal_id, command_id, command).await {
        // Nothing is actually running — don't leave a dangling
        // 'running' row with no agent ever going to report on it, and
        // tell open tabs, which were just told it started (SME-144). Only
        // once the row says so: if the mark fails, the row (and a reload)
        // still say running, so the tabs keep saying it too.
        match db::mark_terminal_command_lost(pool, command_id).await {
            Ok(()) => publish_command_update(conversation_id, terminal_id, command_id, None, "lost"),
            Err(mark) => tracing::error!(command_id, error = %mark, "couldn't mark an unsent command lost"),
        }
        return Err(RunCommandError::Failed(e.to_string()));
    }
    Ok(())
}

/// A command's start (`command` given) or its end without an exit code
/// (`lost`), for the sandbox panel.
fn publish_command_update(conversation_id: i64, terminal_id: i64, command_id: &str, command: Option<&str>, status: &str) {
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::SandboxCommandUpdate {
            terminal_id,
            command_id: command_id.to_string(),
            command: command.map(str::to_string),
            status: status.to_string(),
            exit_code: None,
            stream: None,
            latest_output: None,
            position: None,
        },
    );
}

/// Sends a `signal` — same reconnect-first, never-launches behavior as
/// `send_command`.
pub async fn send_signal(
    pool: &PgPool,
    terminal_id: i64,
    command_id: &str,
    signal: &str,
) -> Result<(), TerminalError> {
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id)
        .await?
        .ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    Ok(conn.send(&ClientMessage::Signal {
        terminal_id: terminal_id.to_string(),
        id: command_id.to_string(),
        signal: signal.to_string(),
    })?)
}

/// Reads (a paginated slice of) `path` in this conversation's pod.
/// Reconnects first if needed, same as `send_command`/`send_signal` —
/// never launches a fresh agent itself.
pub async fn read_file(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    offset: u32,
    limit: u32,
) -> Result<FileContents, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::ReadFile { request_id, path: path.to_string(), offset, limit })
        .await;
    expect_reply(reply, "read_file", |reply| match reply {
        Reply::FileRead(contents) => Some(contents),
        _ => None,
    })
}

/// Creates or overwrites `path` in this conversation's pod. `expected_hash`
/// is `None` only for a brand-new file (the read-before-write check has
/// nothing to have read yet) — see
/// SME-11's "Read-before-write discipline."
/// Returns the new content's hash.
pub async fn write_file(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    content: &str,
    expected_hash: Option<String>,
) -> Result<String, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::WriteFile {
            request_id,
            path: path.to_string(),
            content: content.to_string(),
            expected_hash,
        })
        .await;
    expect_reply(reply, "write_file", |reply| match reply {
        Reply::FileWritten { hash } => Some(hash),
        _ => None,
    })
}

/// Applies a targeted `old_string` → `new_string` replacement to `path` in
/// this conversation's pod. `expected_hash` is always required (unlike
/// `write_file`) — `edit_file` always needs a prior read to have produced
/// the `old_string` it's matching against. `expected_line`, if set, targets
/// one specific occurrence instead of requiring a file-wide unique match —
/// see SME-11's "What" on `edit_file`. Returns the new content's hash.
pub async fn edit_file(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    expected_hash: String,
    expected_line: Option<u32>,
) -> Result<String, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::EditFile {
            request_id,
            path: path.to_string(),
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
            replace_all,
            expected_hash,
            expected_line,
        })
        .await;
    expect_reply(reply, "edit_file", |reply| match reply {
        Reply::FileEdited { hash } => Some(hash),
        _ => None,
    })
}

/// Lists `path` (one level, non-recursive) in this conversation's pod.
pub async fn list_directory(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
) -> Result<Vec<DirEntry>, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::ListDirectory { request_id, path: path.to_string() })
        .await;
    expect_reply(reply, "list_directory", |reply| match reply {
        Reply::DirectoryListed { entries } => Some(entries),
        _ => None,
    })
}

/// Finds files under `path` whose path relative to `path` matches
/// `pattern`, paginated by `offset`/`limit` — see
/// SME-19.
pub async fn glob(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    pattern: &str,
    offset: u32,
    limit: u32,
) -> Result<GlobResult, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::Glob {
            request_id,
            path: path.to_string(),
            pattern: pattern.to_string(),
            offset,
            limit,
        })
        .await;
    expect_reply(reply, "glob", |reply| match reply {
        Reply::GlobMatched(result) => Some(result),
        _ => None,
    })
}

/// Searches file contents under `path` for `pattern`, optionally narrowed
/// to files matching `glob` first, paginated by `offset`/`limit` — see
/// SME-19.
pub async fn grep(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    pattern: &str,
    glob: Option<String>,
    case_insensitive: bool,
    offset: u32,
    limit: u32,
) -> Result<GrepResult, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::Grep {
            request_id,
            path: path.to_string(),
            pattern: pattern.to_string(),
            glob,
            case_insensitive,
            offset,
            limit,
        })
        .await;
    expect_reply(reply, "grep", |reply| match reply {
        Reply::GrepMatched(result) => Some(result),
        _ => None,
    })
}
