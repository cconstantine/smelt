//! The sandbox panel's snapshot: pods, terminals and their commands.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::db;
#[cfg(feature = "server")]
use crate::sandbox;
#[cfg(feature = "server")]
use sqlx::PgPool;
#[cfg(feature = "server")]
use std::collections::HashMap;

/// One line of a command's output, in the order it actually happened —
/// `stdout`/`stderr` fetched and capped independently (see
/// `fetch_command_summary`) but merged back into one true chronological
/// sequence here, rather than the panel showing "all stdout, then all
/// stderr" the way two separate fields would.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxOutputLine {
    pub stream: String,
    pub data: String,
    /// The agent's per-command `seq` (see `SandboxCommandUpdate::position`).
    pub seq: i64,
}

/// One terminal's current/most recent command, hydrated for the sandbox
/// panel's initial scrollback — see SME-10.
/// Only the current/most recent command is included; older history in a
/// terminal stays reachable through the model's own `list_commands`/
/// `read_terminal_output` tools, not duplicated here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxCommandSummary {
    pub command_id: String,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub output: Vec<SandboxOutputLine>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxTerminalSummary {
    pub terminal_id: i64,
    pub pod_id: i64,
    pub status: String,
    /// Most recent `HISTORY_LIMIT` commands, **oldest first** — natural
    /// terminal-scrollback reading order, the newest command (and its
    /// output) at the bottom, closest to where the next command will
    /// appear. `db::list_terminal_commands` itself returns most-recent-
    /// first; this is reversed when the snapshot is built. Older commands
    /// beyond the limit stay reachable through the model's own
    /// `list_commands`/`read_terminal_output` tools, not duplicated here.
    pub commands: Vec<SandboxCommandSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxPodSummary {
    pub pod_id: i64,
    pub status: String,
    pub terminals: Vec<SandboxTerminalSummary>,
    /// Ports the model has shared as previews, with their links (SME-42).
    pub previews: Vec<crate::events::SandboxPreview>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxSnapshot {
    pub pods: Vec<SandboxPodSummary>,
}

/// Last N lines per stream, per command, on initial load, matching
/// `read_terminal_output`'s own tool-facing default — the panel grows from
/// there via live events for as long as the tab stays open, never
/// re-fetching the full history.
#[cfg(feature = "server")]
const SNAPSHOT_TAIL_LINES: i64 = 200;

/// How many of a terminal's most recent commands the panel hydrates on
/// load — matches `list_commands`' own tool-facing default (see
/// `anthropic::tools::DEFAULT_LIST_COMMANDS_LIMIT`).
#[cfg(feature = "server")]
const HISTORY_LIMIT: i64 = 20;

#[cfg(feature = "server")]
async fn fetch_command_summary(
    pool: &PgPool,
    command: &db::TerminalCommand,
) -> Option<SandboxCommandSummary> {
    let status = db::terminal_command_status(pool, &command.command_id)
        .await
        .ok()??;
    let stdout_offset = (status.stdout_lines - SNAPSHOT_TAIL_LINES).max(0);
    let stderr_offset = (status.stderr_lines - SNAPSHOT_TAIL_LINES).max(0);
    let stdout = db::read_terminal_output(
        pool,
        &command.command_id,
        &["stdout"],
        stdout_offset,
        SNAPSHOT_TAIL_LINES,
    )
    .await
    .unwrap_or_default();
    let stderr = db::read_terminal_output(
        pool,
        &command.command_id,
        &["stderr"],
        stderr_offset,
        SNAPSHOT_TAIL_LINES,
    )
    .await
    .unwrap_or_default();
    // stdout/stderr are each capped to their own tail independently (so a
    // stderr spew can't crowd stdout out of the window, or vice versa),
    // which means they arrive as two separately-ordered lists — merge back
    // by `seq` into the order the lines actually happened in.
    let mut output: Vec<db::TerminalLine> = stdout.into_iter().chain(stderr).collect();
    output.sort_by_key(|line| line.seq);
    Some(SandboxCommandSummary {
        command_id: command.command_id.clone(),
        command: command.command.clone(),
        status: status.status,
        exit_code: status.exit_code,
        output: output
            .into_iter()
            .map(|line| SandboxOutputLine {
                stream: line.stream,
                data: line.data,
                seq: line.seq,
            })
            .collect(),
    })
}

#[cfg(feature = "server")]
async fn fetch_terminal_command_history(
    pool: &PgPool,
    terminal_id: i64,
) -> Vec<SandboxCommandSummary> {
    let Ok(recent) = db::list_terminal_commands(pool, terminal_id, HISTORY_LIMIT).await else {
        return Vec::new();
    };
    let mut summaries = Vec::with_capacity(recent.len());
    for command in recent.iter().rev() {
        if let Some(summary) = fetch_command_summary(pool, command).await {
            summaries.push(summary);
        }
    }
    summaries
}

/// Thin wrapper over `sandbox::list_pods`/`sandbox::list_terminals` for the
/// browser — a one-shot pull, not a subscription, same shape as `get_todos`.
/// Used both for the initial sandbox-panel load and for the reconciliation
/// pull `subscribe_conversation_events`'s caller does on connect/reconnect.
///
/// Eagerly attempts `sandbox::try_reconnect` for every pod before reading
/// terminal status — otherwise a pod that survived a smelt restart with a
/// perfectly healthy agent still reports "disconnected" here until the
/// model happens to touch it next, since `list_terminals` itself only ever
/// checks the connection registry, never tries to repair it.
#[get("/api/conversations/{id}/sandbox")]
pub async fn get_sandbox_state(id: i64) -> ServerFnResult<SandboxSnapshot> {
    let pool = db::get();
    let pods = sandbox::list_pods(pool, id)
        .await
        .map_err(ServerFnError::new)?;
    for pod in &pods {
        sandbox::try_reconnect(pool, pod.pod_id).await;
    }
    let terminals = sandbox::list_terminals(pool, id)
        .await
        .map_err(ServerFnError::new)?;

    let mut by_pod: HashMap<i64, Vec<SandboxTerminalSummary>> = HashMap::new();
    for terminal in terminals {
        let commands = fetch_terminal_command_history(pool, terminal.terminal_id).await;
        by_pod
            .entry(terminal.pod_id)
            .or_default()
            .push(SandboxTerminalSummary {
                terminal_id: terminal.terminal_id,
                pod_id: terminal.pod_id,
                status: terminal.status,
                commands,
            });
    }

    // No links when previews are off (a bad `SMELT_PREVIEW_URL`): the
    // model's `sandbox_preview_url` says why when it tries to share one.
    let template = crate::preview::configured_template().ok();
    let mut summaries = Vec::with_capacity(pods.len());
    for pod in pods {
        let previews = match &template {
            Some(template) => {
                let ports = db::list_pod_previews(pool, pod.pod_id)
                    .await
                    .map_err(ServerFnError::new)?;
                crate::preview::preview_links(template, id, &crate::preview::stored_previews(&ports))
            }
            None => Vec::new(),
        };
        summaries.push(SandboxPodSummary {
            pod_id: pod.pod_id,
            status: pod.status,
            terminals: by_pod.remove(&pod.pod_id).unwrap_or_default(),
            previews,
        });
    }
    let pods = summaries;

    Ok(SandboxSnapshot { pods })
}
