//! Sandbox pods, terminals and the commands run in them.

use serde_json::Value;
use sqlx::PgPool;

use super::Tool;
use super::server::{required_i64, required_str};
use crate::{db, sandbox};
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        // --- Terminal: pod, terminal, and command are three separate,
        // explicitly-guarded lifecycles. A conversation has at most one
        // live pod at a time, each with N terminals — see
        // SME-9 and
        // SME-11's "One pod per conversation."
        Tool {
            def: ToolDefinition {
                name: "create_pod".to_string(),
                description: "Create this conversation's sandbox pod. Refuses if one already \
                               exists — call terminate_pod first if you want a fresh one. \
                               Returns the new pod's id. A terminal can't be created until a \
                               pod exists. memory_limit optionally overrides the \
                               deployment's default memory limit for just this one pod (e.g. \
                               memory_limit: \"4Gi\" for a memory-heavy task) — plain Kubernetes \
                               quantity strings, rejected by Kubernetes itself (as an error from \
                               this call) if malformed or over the deployment's configured \
                               ceiling. docker_memory_limit does the same for the \
                               pod's Docker daemon, whose containers share its limit, not the \
                               sandbox's (e.g. docker_memory_limit: \"12Gi\" for a big compose \
                               stack)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "memory_limit": {"type": "string"},
                        "docker_memory_limit": {"type": "string"}
                    }
                }),
            },
            run: run!(|c| create_pod_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "terminate_pod".to_string(),
                description: "Delete this conversation's sandbox pod. Errors if there isn't \
                               one (call create_pod first). Fails if it still has a terminal \
                               in it — call terminate_terminal on it first."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| terminate_pod_tool(c.pool, c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "list_pods".to_string(),
                description: "List this conversation's sandbox pod, if it has one, with its \
                               id and status."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| list_pods_tool(c.pool, c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "create_terminal".to_string(),
                description: "Create a new persistent terminal inside this conversation's \
                               sandbox pod. Requires that pod to already exist (create_pod \
                               first). Not idempotent — every call creates a genuinely new \
                               terminal; call list_terminals to see what already exists before \
                               deciding you need another. Returns the new terminal's id. This \
                               is a real, persistent shell: state (working directory, exported \
                               variables) persists across separate run_terminal_command calls \
                               and across turns. Multiple terminals in the same pod share that \
                               pod's filesystem and installed state, but are otherwise \
                               independent — each has its own shell state, and a long-running \
                               command in one never blocks another."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| create_terminal_tool(c.pool, c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "terminate_terminal".to_string(),
                description: "End a terminal without deleting its pod or affecting any other \
                               terminal in it. Idempotent if it's already terminated; errors \
                               if terminal_id is unknown. Fails if a command is still running \
                               in it — send_signal or wait for it to finish first."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"terminal_id": {"type": "integer"}},
                    "required": ["terminal_id"]
                }),
            },
            run: run!(|c| terminate_terminal_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "list_terminals".to_string(),
                description: "List every terminal that currently exists (and whether its \
                               pod's connection is reachable) across every pod in this \
                               conversation, each with its id and which pod it's in."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            run: run!(|c| list_terminals_tool(c.pool, c.conversation_id)),
        },
        Tool {
            def: ToolDefinition {
                name: "run_terminal_command".to_string(),
                description: "Run a command in the given terminal. Requires that terminal to \
                               already exist (create_terminal first). Starts the command in \
                               the background and returns immediately with a command_id — \
                               never the command's own output. Only one command may be in \
                               flight per terminal at a time; this errors if another is still \
                               running in that terminal (a different terminal is unaffected). \
                               Use terminal_command_status/read_terminal_output with the \
                               returned id to check on it, or send_signal to interrupt it — \
                               you'll also be notified here when it finishes, with no further \
                               tool call needed. If a command seems stuck, send_signal it (or \
                               use another terminal) rather than retrying this one."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "terminal_id": {"type": "integer"},
                        "command": {"type": "string", "description": "the shell command to run"}
                    },
                    "required": ["terminal_id", "command"]
                }),
            },
            run: run!(|c| run_terminal_command_tool(c.pool, c.conversation_id, c.tool_use_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "send_signal".to_string(),
                description: "Send a signal to the currently-running command — like hitting \
                               Ctrl-C in a real terminal for INT. The command may die, \
                               handle it gracefully, or ignore it entirely; the terminal \
                               itself (working directory, environment) is never affected \
                               either way. Errors if command_id doesn't match the command \
                               currently in flight."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command_id": {"type": "string"},
                        "signal": {"type": "string", "enum": ["INT", "TERM", "KILL"]}
                    },
                    "required": ["command_id", "signal"]
                }),
            },
            run: run!(|c| send_signal_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "terminal_command_status".to_string(),
                description: "Check a terminal command's status without blocking — \
                               \"running\", or finished/lost with its exit code — plus how \
                               many lines of stdout and stderr it has produced so far (use \
                               these counts with read_terminal_output's offset/limit)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"command_id": {"type": "string"}},
                    "required": ["command_id"]
                }),
            },
            run: run!(|c| terminal_command_status_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "read_terminal_output".to_string(),
                description: "Read a bounded slice of a terminal command's output — never \
                               the whole thing at once. Line numbers (offset/limit) are \
                               relative to whichever stream(s) you request: offset 0 against \
                               \"stdout\" is not necessarily the same line as offset 0 \
                               against \"both\". Lines over 2000 characters are cut; when the \
                               slice is too big to return whole, it stops early with \
                               truncated: true and the next_offset to read from."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command_id": {"type": "string"},
                        "stream": {"type": "string", "enum": ["stdout", "stderr", "both"], "description": "defaults to \"both\""},
                        "offset": {"type": "integer", "minimum": 0, "description": "defaults to 0"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "defaults to 200, capped at 500"}
                    },
                    "required": ["command_id"]
                }),
            },
            run: run!(|c| read_terminal_output_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "list_commands".to_string(),
                description: "List the most recent commands run in the given terminal — like \
                               `ps`, but includes finished and lost ones too, and survives \
                               that terminal since being terminated. Most-recent-first, \
                               bounded."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "terminal_id": {"type": "integer"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "defaults to 20, capped at 50"}
                    },
                    "required": ["terminal_id"]
                }),
            },
            run: run!(|c| list_commands_tool(c.pool, c.conversation_id, c.input)),
        },
    ]
}

// The tools: thin wrappers over sandbox.rs (pod/terminal lifecycle + agent
// connection) and db.rs (command bookkeeping + output). No locking of
// their own: `execute` is only ever called from `run_turn`'s tool-
// dispatch loop, which already holds `conversation_id`'s lock for the
// whole turn (including every tool_use block in it, processed one at a
// time, never concurrently) — see SME-9's "Which files" bullet on
// `anthropic/tools.rs` and `api::chat::run_turn`'s own `conversation_lock`.

async fn create_pod_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let pod_id = sandbox::create_pod(pool, conversation_id, pod_limit_overrides(input))
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"pod_id": pod_id}).to_string())
}

/// `create_pod`'s optional memory limit overrides, each a Kubernetes
/// quantity string left for Kubernetes itself to validate. A
/// `cpu_limit`/`docker_cpu_limit` an older call still passes is
/// ignored: sandbox pods have no CPU limit (SME-77).
pub(super) fn pod_limit_overrides(input: &Value) -> sandbox::PodLimitOverrides {
    let field = |name| input.get(name).and_then(Value::as_str).map(str::to_string);
    sandbox::PodLimitOverrides {
        memory: field("memory_limit"),
        docker_memory: field("docker_memory_limit"),
    }
}

async fn terminate_pod_tool(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    sandbox::terminate_pod(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok("pod terminated".to_string())
}

async fn list_pods_tool(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    let pods = sandbox::list_pods(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let payload: Vec<_> = pods
        .iter()
        .map(|p| {
            serde_json::json!({
                "pod_id": p.pod_id,
                "status": p.status,
                "agent": p.agent.as_ref().map(|agent| agent.describe()),
            })
        })
        .collect();
    Ok(serde_json::json!({"pods": payload}).to_string())
}

async fn create_terminal_tool(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    let terminal_id = sandbox::create_terminal(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"terminal_id": terminal_id}).to_string())
}

/// Refuses a terminal that isn't in one of `conversation_id`'s pods, as
/// unknown: terminal ids are sequential, so another conversation's are
/// easy to guess (SME-51 B2).
async fn owned_terminal(pool: &PgPool, conversation_id: i64, terminal_id: i64) -> Result<(), String> {
    match db::terminal_conversation_id(pool, terminal_id).await.map_err(|e| e.to_string())? {
        Some(owner) if owner == conversation_id => Ok(()),
        _ => Err(format!("unknown terminal id: {terminal_id}")),
    }
}

/// `command_id`'s command if it ran in `conversation_id`; another
/// conversation's is reported as unknown (SME-51 B2).
async fn owned_command(
    pool: &PgPool,
    conversation_id: i64,
    command_id: &str,
) -> Result<db::TerminalCommand, String> {
    db::get_terminal_command(pool, command_id)
        .await
        .map_err(|e| e.to_string())?
        .filter(|command| command.conversation_id == conversation_id)
        .ok_or_else(|| format!("unknown command id: {command_id}"))
}

async fn terminate_terminal_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let terminal_id = required_i64(input, "terminal_id")?;
    owned_terminal(pool, conversation_id, terminal_id).await?;
    match sandbox::terminate_terminal(pool, terminal_id).await {
        Ok(()) => Ok(format!("terminal {terminal_id} terminated")),
        Err(sandbox::TerminalError::CommandStillRunning) => {
            Err(busy_terminal_error(pool, terminal_id).await)
        }
        Err(e) => Err(e.to_string()),
    }
}

async fn list_terminals_tool(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    let terminals = sandbox::list_terminals(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let payload: Vec<_> = terminals
        .iter()
        .map(|t| serde_json::json!({"terminal_id": t.terminal_id, "pod_id": t.pod_id, "status": t.status}))
        .collect();
    Ok(serde_json::json!({"terminals": payload}).to_string())
}

/// The refusal when `command` still holds its terminal: which command,
/// for how long, and the ways out (SME-76: a bare "already running"
/// left a model retrying the busy terminal for hours).
pub(super) fn busy_terminal_message(command: &db::TerminalCommand, now: chrono::NaiveDateTime) -> String {
    const SHOWN_CHARS: usize = 80;
    let shown: String = command.command.chars().take(SHOWN_CHARS).collect();
    let ellipsis = if command.command.chars().count() > SHOWN_CHARS { "…" } else { "" };
    let minutes = (now - command.created_at).num_minutes().max(0);
    format!(
        "a command is already running in this terminal: {id} (\"{shown}{ellipsis}\"), started \
         {minutes} min ago with no exit yet. Read its output with read_terminal_output, \
         interrupt it with send_signal (command_id {id}, INT, then KILL if it ignores that), \
         or run this in another terminal (create_terminal).",
        id = command.command_id,
    )
}

/// `busy_terminal_message` for whatever command holds `terminal_id`.
async fn busy_terminal_error(pool: &PgPool, terminal_id: i64) -> String {
    match db::terminal_command_is_running(pool, terminal_id).await {
        Ok(Some(command)) => busy_terminal_message(&command, chrono::Utc::now().naive_utc()),
        Ok(None) => "the command running in this terminal has just finished; try again".to_string(),
        Err(e) => e.to_string(),
    }
}

/// Reuses `tool_use_id` as `command_id`: the id already exists, so
/// there's no need to mint a new one.
async fn run_terminal_command_tool(
    pool: &PgPool,
    conversation_id: i64,
    tool_use_id: &str,
    input: &Value,
) -> Result<String, String> {
    let terminal_id = required_i64(input, "terminal_id")?;
    let command = required_str(input, "command")?;
    owned_terminal(pool, conversation_id, terminal_id).await?;

    let command_id = tool_use_id.to_string();
    sandbox::run_command(pool, conversation_id, terminal_id, &command_id, &command)
        .await
        .map_err(|e| match e {
            sandbox::RunCommandError::Busy(running) => busy_terminal_message(&running, chrono::Utc::now().naive_utc()),
            sandbox::RunCommandError::Failed(e) => e,
        })?;
    Ok(format!("command sent (id: {command_id})"))
}

const ALLOWED_SIGNALS: &[&str] = &["INT", "TERM", "KILL"];

async fn send_signal_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let command_id = required_str(input, "command_id")?;
    let signal = required_str(input, "signal")?;
    if !ALLOWED_SIGNALS.contains(&signal.as_str()) {
        return Err(format!(
            "signal must be one of {ALLOWED_SIGNALS:?}, got {signal}"
        ));
    }
    let command = owned_command(pool, conversation_id, &command_id).await?;
    sandbox::send_signal(pool, command.terminal_id, &command_id, &signal)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("signal {signal} sent to {command_id}"))
}

async fn terminal_command_status_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let command_id = required_str(input, "command_id")?;
    owned_command(pool, conversation_id, &command_id).await?;
    let status = db::terminal_command_status(pool, &command_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("unknown command id: {command_id}"))?;
    Ok(serde_json::json!({
        "status": status.status,
        "exit_code": status.exit_code,
        "stdout_lines": status.stdout_lines,
        "stderr_lines": status.stderr_lines,
    })
    .to_string())
}

/// A line longer than this is cut, saying so, by `fit_lines`.
pub(super) const MAX_LINE_CHARS: usize = 2_000;

/// How much of `MAX_TOOL_RESULT_CHARS` line-based reads fill with lines
/// (JSON-encoded), leaving room for the rest of their response, so
/// they stop at a whole line with a next offset instead of being cut.
pub(super) const LINES_BUDGET_CHARS: usize = 25_000;

/// Cuts each line to `MAX_LINE_CHARS` and keeps lines while their
/// JSON-encoded size, plus `per_line` for whatever the response wraps
/// each one in, fits `budget` (always at least one). Returns the kept
/// lines and whether any were left out for the budget.
pub(super) fn fit_lines(lines: Vec<String>, budget: usize, per_line: usize) -> (Vec<String>, bool) {
    let total = lines.len();
    let mut kept = Vec::new();
    let mut used = 0;
    for line in lines {
        let line = match crate::fetch_guard::truncate(line, MAX_LINE_CHARS) {
            (cut, true) => format!("{cut}… [line cut at {MAX_LINE_CHARS} chars]"),
            (whole, false) => whole,
        };
        let size = serde_json::to_string(&line).map_or(line.len(), |s| s.len()) + per_line;
        if !kept.is_empty() && used + size > budget {
            break;
        }
        used += size;
        kept.push(line);
    }
    let truncated = kept.len() < total;
    (kept, truncated)
}

const DEFAULT_READ_LIMIT: i64 = 200;

const MAX_READ_LIMIT: i64 = 500;

async fn read_terminal_output_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let command_id = required_str(input, "command_id")?;
    owned_command(pool, conversation_id, &command_id).await?;
    let stream = input
        .get("stream")
        .and_then(Value::as_str)
        .unwrap_or("both");
    let streams: &[&str] = match stream {
        "stdout" => &["stdout"],
        "stderr" => &["stderr"],
        "both" => &["stdout", "stderr"],
        other => {
            return Err(format!(
                "stream must be one of stdout, stderr, both — got {other}"
            ));
        }
    };
    let offset = input
        .get("offset")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let limit = input
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_READ_LIMIT)
        .clamp(1, MAX_READ_LIMIT);

    let lines = db::read_terminal_output(pool, &command_id, streams, offset, limit)
        .await
        .map_err(|e| e.to_string())?;
    let (streams, data): (Vec<String>, Vec<String>) =
        lines.into_iter().map(|l| (l.stream, l.data)).unzip();
    // `{"stream":"stdout","data":},`
    const PER_LINE: usize = 30;
    let (data, truncated) = fit_lines(data, LINES_BUDGET_CHARS, PER_LINE);
    let payload: Vec<_> = streams
        .into_iter()
        .zip(data)
        .map(|(stream, data)| serde_json::json!({"stream": stream, "data": data}))
        .collect();
    let returned = payload.len();
    let mut response = serde_json::json!({"lines": payload, "returned": returned});
    if truncated {
        response["truncated"] = Value::Bool(true);
        response["next_offset"] = Value::from(offset + returned as i64);
    }
    Ok(response.to_string())
}

const DEFAULT_LIST_COMMANDS_LIMIT: i64 = 20;

const MAX_LIST_COMMANDS_LIMIT: i64 = 50;

async fn list_commands_tool(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let terminal_id = required_i64(input, "terminal_id")?;
    owned_terminal(pool, conversation_id, terminal_id).await?;
    let limit = input
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_LIST_COMMANDS_LIMIT)
        .clamp(1, MAX_LIST_COMMANDS_LIMIT);
    let commands = db::list_terminal_commands(pool, terminal_id, limit)
        .await
        .map_err(|e| e.to_string())?;
    let payload: Vec<_> = commands
        .iter()
        .map(|c| {
            serde_json::json!({
                "command_id": c.command_id,
                "command": c.command,
                "status": c.status,
                "exit_code": c.exit_code,
            })
        })
        .collect();
    Ok(serde_json::json!({"commands": payload}).to_string())
}
