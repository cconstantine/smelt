//! In-process tool implementations dispatched by name. See
//! `docs/projects/plans/tool-use-round-trip.md` for the full design — `add`
//! and `count` are deliberately throwaway stand-ins proving the Anthropic
//! tool-use protocol round-trips through this codebase, not real tools.
//! `run_async` and the task-management suite (`list_tasks`, `task_status`,
//! `task_output`, `task_result`, `wait_task`, `cancel_task`) are the
//! reusable pieces: a generic mechanism for running any tool call
//! asynchronously, modeled on OS process management (`ps`/`wait`/`kill`).

use serde::{Deserialize, Serialize};

/// Read-only snapshot of one task for the *browser* (`get_tasks`, a plain
/// server function) — shares the same filter-by-`conversation_id` logic
/// `list_tasks` (the model-facing tool) uses, rather than two copies of it.
/// Defined outside the `server`-gated module below since, unlike `execute`,
/// this type itself crosses the client/server boundary as a server-function
/// return value and so must compile on the `web` target too.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskSummary {
    pub task_id: String,
    pub tool: String,
    pub status: String,
    /// Full accumulated output so far, for hydrating a task's terminal
    /// widget on initial load or reconnect — a live `TaskUpdate` only ever
    /// carries the one new line, so the one-shot snapshot pull is the only
    /// source for everything that streamed before a browser tab connected.
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

#[cfg(feature = "server")]
mod server {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::Duration;

    use serde_json::Value;
    use sqlx::PgPool;
    use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
    use tokio::task::AbortHandle;

    use super::TaskSummary;
    use crate::anthropic::{AnthropicMessage, ContentBlock};
    use crate::{api::chat, events};

    /// Runs the named tool against `input`, returning its result as a plain
    /// string on success or an error message on failure — the caller (the
    /// `send_message` turn loop) wraps either into a `ContentBlock::ToolResult`.
    /// `tool_use_id` is only meaningful to the `run_async` branch (it reuses
    /// that id as the spawned task's id — "id already exists, don't mint a
    /// new one," the same pattern used elsewhere in this codebase); every
    /// other tool ignores it. `pool` is threaded through (rather than reached
    /// for via `db::get()` directly) so `run_async`'s spawned background
    /// task can carry the *same* pool its caller used — this is what lets
    /// tests use an isolated `#[sqlx::test]` pool end-to-end instead of the
    /// process-global one.
    pub async fn execute(
        pool: &PgPool,
        conversation_id: i64,
        tool_use_id: &str,
        name: &str,
        input: &Value,
    ) -> Result<String, String> {
        match name {
            "run_async" => run_async(pool, conversation_id, tool_use_id, input).await,
            "list_tasks" => Ok(list_tasks(conversation_id)),
            "task_status" => task_status_tool(input),
            "task_stdout" => task_stdout_tool(input),
            "task_stderr" => task_stderr_tool(input),
            "task_result" => task_result_tool(input),
            "wait_task" => wait_task_tool(input).await,
            "cancel_task" => cancel_task_tool(conversation_id, input).await,
            "write_task_stdin" => write_task_stdin_tool(input),
            _ => execute_synchronous(name, input).await,
        }
    }

    /// Tool schemas offered to the model on every `send_message` call, and
    /// also what `run_async` validates a wrapped tool's input against
    /// before spawning (see `validate_against_schema`) — one definition,
    /// used both ways, so there's a single source of truth for what a tool
    /// accepts rather than the schema and a hand-written check drifting
    /// apart. `add` and `count` are deliberately throwaway stand-ins
    /// proving the tool-use protocol round trips through this codebase,
    /// not real tools.
    pub fn tool_definitions() -> Vec<crate::anthropic::ToolDefinition> {
        use crate::anthropic::ToolDefinition;
        vec![
            ToolDefinition {
                name: "add".to_string(),
                description: "Add two numbers and return their sum.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "a": {"type": "number"},
                        "b": {"type": "number"}
                    },
                    "required": ["a", "b"]
                }),
            },
            ToolDefinition {
                name: "count".to_string(),
                description: "Count from 1 up to target, pausing interval_seconds between \
                               increments. Deliberately slow — demonstrates a tool call that \
                               takes real time to complete. Call this directly for the \
                               ordinary, blocking behavior; call it via run_async instead to \
                               run it in the background."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "integer", "minimum": 1, "maximum": 1000},
                        "interval_seconds": {"type": "number", "minimum": 0, "maximum": 60}
                    },
                    "required": ["target", "interval_seconds"]
                }),
            },
            ToolDefinition {
                name: "echo".to_string(),
                description: "Reads lines from its own stdin and, for each one, writes a \
                               short diagnostic to stderr then echoes the line itself to \
                               stdout — stops once timeout_seconds passes with no new input. \
                               Only useful run via run_async plus write_task_stdin: called \
                               directly (or with nothing ever written to its stdin) it has \
                               no input to read and returns immediately. Demonstrates that a \
                               background task's stdin/stdout/stderr all round-trip \
                               independently, the same three streams a process has."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 60}
                    },
                    "required": ["timeout_seconds"]
                }),
            },
            ToolDefinition {
                name: "run_async".to_string(),
                description: "Start another tool (add, count, or echo) running in the \
                               background and return immediately with a task id, instead of \
                               waiting for it to finish — like fork+exec for a tool call. \
                               You'll be notified in this conversation when the task \
                               finishes, with no further tool call needed; use list_tasks/\
                               task_status/task_stdout/task_stderr/task_result/wait_task/\
                               cancel_task/write_task_stdin with the returned task id to \
                               check in sooner or manage it yourself."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string", "enum": ["add", "count", "echo"], "description": "which tool to run in the background"},
                        "input": {"type": "object", "description": "the input that tool normally takes"},
                        "stream_output": {"type": "boolean", "description": "if true, push every stdout/stderr line to this conversation as it's produced, not just the final result (expensive — one model call per line)"}
                    },
                    "required": ["tool", "input"]
                }),
            },
            ToolDefinition {
                name: "list_tasks".to_string(),
                description: "List every background task started via run_async in this \
                              conversation, with its current status — like `ps`."
                    .to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "task_status".to_string(),
                description: "Check a background task's status without blocking — \
                              \"running\"/\"finished\"/\"failed\"/\"cancelled\" — like a \
                              non-blocking `ps <pid>`."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"task_id": {"type": "string"}},
                    "required": ["task_id"]
                }),
            },
            ToolDefinition {
                name: "task_stdout".to_string(),
                description: "Read a background task's accumulated stdout so far — like \
                              `tail`."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"task_id": {"type": "string"}},
                    "required": ["task_id"]
                }),
            },
            ToolDefinition {
                name: "task_stderr".to_string(),
                description: "Read a background task's accumulated stderr so far — like \
                              `task_stdout`, but for diagnostic/error output kept separate \
                              from a task's real output."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"task_id": {"type": "string"}},
                    "required": ["task_id"]
                }),
            },
            ToolDefinition {
                name: "write_task_stdin".to_string(),
                description: "Write a line of input to a running background task's stdin — \
                               like piping into a process. Only tools that actually read \
                               their stdin (currently just echo) will do anything with it; \
                               others ignore it, the same as writing to a process that never \
                               reads its stdin. Errors if the task is unknown or has already \
                               finished/failed/been cancelled."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string"},
                        "data": {"type": "string", "description": "the line to write"}
                    },
                    "required": ["task_id", "data"]
                }),
            },
            ToolDefinition {
                name: "task_result".to_string(),
                description: "Read a background task's final return value once it has \
                              finished. Errors if it's still running, failed, or was \
                              cancelled."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"task_id": {"type": "string"}},
                    "required": ["task_id"]
                }),
            },
            ToolDefinition {
                name: "wait_task".to_string(),
                description: "Block and wait (up to timeout_seconds) for a background task \
                              to finish, then return its result — like `waitpid`/`select` \
                              with a timeout. Returns a timeout error if it's still running \
                              when the timeout elapses; call again to keep waiting."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 120}
                    },
                    "required": ["task_id", "timeout_seconds"]
                }),
            },
            ToolDefinition {
                name: "cancel_task".to_string(),
                description: "Best-effort abort a still-running background task — like \
                              `kill`. A no-op (not an error) if it's already finished, \
                              failed, or cancelled."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"task_id": {"type": "string"}},
                    "required": ["task_id"]
                }),
            },
        ]
    }

    /// Validates `input` against a JSON Schema `schema`, generically for
    /// any tool — the model-facing `input_schema` a `ToolDefinition`
    /// already carries *is* the single source of truth for what a tool
    /// accepts, so this is what `run_async` checks a wrapped tool's input
    /// against before spawning, rather than `run_async` needing to know
    /// each tool's specific fields by hand. Supports the subset of JSON
    /// Schema this codebase's own tool schemas actually use — object type,
    /// `properties` (each with `type`/`minimum`/`maximum`/`enum`), and
    /// `required`. Not a general-purpose validator (no `$ref`, no nested
    /// object schemas, no `oneOf`/`anyOf`/`pattern`); extend it if a future
    /// tool's schema genuinely needs more, or reach for a real JSON Schema
    /// crate at that point rather than growing this indefinitely.
    fn validate_against_schema(input: &Value, schema: &Value) -> Result<(), String> {
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Ok(()); // nothing this validator knows how to check
        }
        if !input.is_object() {
            return Err("expected a JSON object".to_string());
        }

        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if input.get(field).is_none() {
                    return Err(format!("missing required field: {field}"));
                }
            }
        }

        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property_schema) in properties {
                if let Some(value) = input.get(name) {
                    validate_property(name, value, property_schema)?;
                }
            }
        }

        Ok(())
    }

    fn validate_property(name: &str, value: &Value, schema: &Value) -> Result<(), String> {
        if let Some(expected_type) = schema.get("type").and_then(Value::as_str) {
            let matches_type = match expected_type {
                "integer" => value.as_f64().is_some_and(|f| f.fract() == 0.0),
                "number" => value.is_number(),
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                _ => true, // unrecognized type keyword — nothing to check
            };
            if !matches_type {
                return Err(format!(
                    "field {name} must be of type {expected_type}, got {value}"
                ));
            }
        }

        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
            if let Some(actual) = value.as_f64() {
                if actual < minimum {
                    return Err(format!("field {name} must be >= {minimum}, got {actual}"));
                }
            }
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
            if let Some(actual) = value.as_f64() {
                if actual > maximum {
                    return Err(format!("field {name} must be <= {maximum}, got {actual}"));
                }
            }
        }

        if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
            if !allowed.contains(value) {
                return Err(format!(
                    "field {name} must be one of {allowed:?}, got {value}"
                ));
            }
        }

        Ok(())
    }

    /// The wrappable, fully-synchronous tools — the only ones `run_async` is
    /// allowed to name.
    const WRAPPABLE_TOOLS: &[&str] = &["add", "count", "echo"];

    async fn execute_synchronous(name: &str, input: &Value) -> Result<String, String> {
        match name {
            "add" => add(input),
            "count" => count(input).await,
            "echo" => echo(input).await,
            other => Err(format!("unknown tool: {other}")),
        }
    }

    fn required_f64(input: &Value, field: &str) -> Result<f64, String> {
        input
            .get(field)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("missing or non-numeric field: {field}"))
    }

    fn required_str(input: &Value, field: &str) -> Result<String, String> {
        input
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("missing or non-string field: {field}"))
    }

    fn add(input: &Value) -> Result<String, String> {
        let a = required_f64(input, "a")?;
        let b = required_f64(input, "b")?;
        Ok((a + b).to_string())
    }

    /// Clamp bounds for `count`'s `target`/`interval_seconds` — lifted well
    /// past the original "clearly a toy" 1..=5 so `count` can run a
    /// genuinely long, real demo (e.g. counting to a few hundred) if asked,
    /// while still bounded so a call can't pin a task open indefinitely.
    /// Still guessed defaults, not confirmed-settled values — see the
    /// plan's "Open questions."
    const COUNT_TARGET_RANGE: std::ops::RangeInclusive<u64> = 1..=1000;
    /// `interval_seconds` is a `number` in the schema (not `integer`), so
    /// fractional pauses are genuinely supported now, not just accepted and
    /// truncated.
    const COUNT_INTERVAL_RANGE: std::ops::RangeInclusive<f64> = 0.0..=60.0;

    tokio::task_local! {
        /// Set only while running inside a `run_async`-spawned task; absent
        /// for a direct call. The *one* concession `count` makes to being
        /// wrappable — see the plan's "How" section.
        static CURRENT_TASK: String;
    }

    async fn count(input: &Value) -> Result<String, String> {
        let target = required_f64(input, "target")? as u64;
        let interval_seconds = required_f64(input, "interval_seconds")?;

        if !COUNT_TARGET_RANGE.contains(&target) {
            return Err(format!(
                "target must be between {} and {}",
                COUNT_TARGET_RANGE.start(),
                COUNT_TARGET_RANGE.end()
            ));
        }
        if !COUNT_INTERVAL_RANGE.contains(&interval_seconds) {
            return Err(format!(
                "interval_seconds must be between {} and {}",
                COUNT_INTERVAL_RANGE.start(),
                COUNT_INTERVAL_RANGE.end()
            ));
        }

        for i in 1..=target {
            if i > 1 {
                tokio::time::sleep(Duration::from_secs_f64(interval_seconds)).await;
            }
            if let Ok(task_id) = CURRENT_TASK.try_with(|id| id.clone()) {
                record_task_line(&task_id, Stream::Stdout, format!("count: {i}/{target}")).await;
            }
        }

        Ok(format!("Counted to {target}"))
    }

    /// Clamp for `echo`'s `timeout_seconds` — same shape as `wait_task`'s
    /// own bound, since both are "how long to wait before giving up."
    const ECHO_TIMEOUT_RANGE: std::ops::RangeInclusive<u64> = 1..=60;

    /// The one demo tool that actually reads stdin — `add`/`count` never
    /// do, so without this, `write_task_stdin` would have nothing to prove
    /// it works. Only meaningful when run via `run_async` (a direct call
    /// has no task id, hence no stdin to read, and returns immediately);
    /// reads lines from its own task's stdin one at a time, logging a
    /// short diagnostic to stderr for each one received and then echoing
    /// the line itself to stdout — proving both streams round-trip
    /// independently, not just stdout. Stops once `timeout_seconds` passes
    /// with no new input; there's no explicit "close stdin" signal in this
    /// design, matching `wait_task`'s own "everything here is
    /// timeout-bounded, not event-driven" philosophy.
    async fn echo(input: &Value) -> Result<String, String> {
        let timeout_seconds = required_f64(input, "timeout_seconds")? as u64;
        if !ECHO_TIMEOUT_RANGE.contains(&timeout_seconds) {
            return Err(format!(
                "timeout_seconds must be between {} and {}",
                ECHO_TIMEOUT_RANGE.start(),
                ECHO_TIMEOUT_RANGE.end()
            ));
        }

        let Ok(task_id) = CURRENT_TASK.try_with(|id| id.clone()) else {
            return Ok(
                "echo has no stdin to read outside of run_async — nothing to echo".to_string(),
            );
        };

        let mut echoed = 0u32;
        loop {
            let stdin_rx = {
                let tasks = lock_tasks();
                let Some(task) = tasks.get(&task_id) else {
                    break;
                };
                task.stdin_rx.clone()
            };

            let next = {
                let mut rx = stdin_rx.lock().await;
                tokio::time::timeout(Duration::from_secs(timeout_seconds), rx.recv()).await
            };

            match next {
                Ok(Some(line)) => {
                    record_task_line(
                        &task_id,
                        Stream::Stderr,
                        format!("echo: received {} byte(s) of input", line.len()),
                    )
                    .await;
                    record_task_line(&task_id, Stream::Stdout, line).await;
                    echoed += 1;
                }
                Ok(None) => break, // stdin closed (every sender dropped)
                Err(_) => break,   // timed out waiting for the next line
            }
        }

        Ok(format!("echoed {echoed} line(s) from stdin"))
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum TaskStatus {
        Running,
        Finished,
        Failed,
        Cancelled,
    }

    fn status_str(status: TaskStatus) -> &'static str {
        match status {
            TaskStatus::Running => "running",
            TaskStatus::Finished => "finished",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    /// Which of a task's two output logs a line belongs to — mirrors a
    /// process's own stdout/stderr split. `count` only ever writes
    /// `Stdout`; `echo` writes both (a `Stderr` diagnostic per line
    /// received, then the line itself to `Stdout`), specifically to prove
    /// both round-trip independently.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Stream {
        Stdout,
        Stderr,
    }

    impl Stream {
        fn label(self) -> &'static str {
            match self {
                Stream::Stdout => "stdout",
                Stream::Stderr => "stderr",
            }
        }
    }

    struct Task {
        conversation_id: i64,
        tool: String,
        stream_output: bool,
        status: TaskStatus,
        stdout: Vec<String>,
        stderr: Vec<String>,
        result: Option<Result<String, String>>,
        abort: AbortHandle,
        notify: Arc<Notify>,
        /// The same pool the `run_async` call that started this task was
        /// given — carried here so the spawned background task (and this
        /// task's later output/completion pushes) use that exact pool
        /// rather than reaching for a process-global one.
        pool: PgPool,
        /// The write end a `write_task_stdin` call sends into — process-like:
        /// writing succeeds regardless of whether anything is actually
        /// reading (same as piping into a process that ignores its stdin),
        /// so this never blocks or errors on the write side.
        stdin_tx: mpsc::UnboundedSender<String>,
        /// The read end — wrapped for interior mutability + cheap cloning
        /// so a tool (looked up by task id, the same pattern `pool` already
        /// uses) can `.lock().await` and `.recv().await` on it without
        /// holding the registry's own (synchronous) lock across an await.
        stdin_rx: Arc<AsyncMutex<mpsc::UnboundedReceiver<String>>>,
    }

    /// Deliberately in-memory and lost on restart — consistent with this
    /// file's already-provisional, deleted-once-real-tools-land status (see
    /// the plan's Open questions).
    static TASKS: LazyLock<Mutex<HashMap<String, Task>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn lock_tasks() -> std::sync::MutexGuard<'static, HashMap<String, Task>> {
        TASKS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `run_async(tool, input, stream_output = false) -> String` — the
    /// generic wrapper, `fork`+`exec` for tool calls. Validates `tool` names
    /// a real, non-`run_async`, non-`task_*` tool, then spawns the named
    /// tool's real (synchronous) implementation on a background task and
    /// returns a task id immediately, without waiting for it.
    async fn run_async(
        pool: &PgPool,
        conversation_id: i64,
        tool_use_id: &str,
        input: &Value,
    ) -> Result<String, String> {
        let tool = required_str(input, "tool")?;
        if !WRAPPABLE_TOOLS.contains(&tool.as_str()) {
            return Err(format!(
                "tool '{tool}' cannot be run via run_async (unknown or not wrappable)"
            ));
        }
        let tool_input = input
            .get("input")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new()));

        // Validate the wrapped tool's input against its own schema *before*
        // reporting "started" — without this, an invalid call (e.g.
        // count's target out of range) would spawn a task doomed to fail,
        // return a misleading success message, and only surface the real
        // error later via task_result/wait_task or a push notification.
        let definition = tool_definitions()
            .into_iter()
            .find(|def| def.name == tool)
            .expect("WRAPPABLE_TOOLS names are always present in tool_definitions()");
        validate_against_schema(&tool_input, &definition.input_schema)
            .map_err(|e| format!("invalid input for {tool}: {e}"))?;

        let stream_output = input
            .get("stream_output")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let task_id = tool_use_id.to_string();
        let notify = Arc::new(Notify::new());
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<String>();

        let spawn_pool = pool.clone();
        let spawn_task_id = task_id.clone();
        let spawn_tool = tool.clone();
        let join_handle = tokio::spawn(async move {
            let result = CURRENT_TASK
                .scope(
                    spawn_task_id.clone(),
                    execute_synchronous(&spawn_tool, &tool_input),
                )
                .await;

            {
                let mut tasks = lock_tasks();
                if let Some(task) = tasks.get_mut(&spawn_task_id) {
                    task.status = match &result {
                        Ok(_) => TaskStatus::Finished,
                        Err(_) => TaskStatus::Failed,
                    };
                    task.result = Some(result.clone());
                    task.notify.notify_waiters();
                }
            }

            match result {
                Ok(output) => {
                    push_terminal_notification(&spawn_pool, &spawn_task_id, "finished", &output)
                        .await
                }
                Err(err) => {
                    push_terminal_notification(&spawn_pool, &spawn_task_id, "failed", &err).await
                }
            }
        });

        {
            let mut tasks = lock_tasks();
            tasks.insert(
                task_id.clone(),
                Task {
                    conversation_id,
                    tool: tool.clone(),
                    stream_output,
                    status: TaskStatus::Running,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    result: None,
                    abort: join_handle.abort_handle(),
                    notify,
                    pool: pool.clone(),
                    stdin_tx,
                    stdin_rx: Arc::new(AsyncMutex::new(stdin_rx)),
                },
            );
        }

        // Published immediately, before the spawned task has produced
        // anything, so the browser sees "started" the instant the task
        // exists.
        events::publish(
            conversation_id,
            events::ConversationEvent::TaskUpdate {
                task_id: task_id.clone(),
                tool: tool.clone(),
                status: "running".to_string(),
                stream: None,
                latest_output: None,
            },
        );

        Ok(format!(
            "Started task {task_id} running {tool}; use list_tasks/task_status/task_stdout/\
             task_stderr/task_result/wait_task/cancel_task/write_task_stdin with this id — \
             you'll also be notified here when it finishes."
        ))
    }

    /// Called whenever a wrapped tool writes a line to its own stdout or
    /// stderr (`count` only ever writes `Stdout`; `echo` writes both):
    /// records the line in the right log, unconditionally publishes a
    /// `TaskUpdate` for the browser (cheap, in-process, no network cost),
    /// and — only if `stream_output` is set for this task — additionally
    /// pushes the line to the conversation as a real turn via
    /// `chat::run_turn`, the expensive, model-facing path.
    async fn record_task_line(task_id: &str, stream: Stream, line: String) {
        let Some((conversation_id, tool, stream_output, pool)) = ({
            let mut tasks = lock_tasks();
            tasks.get_mut(task_id).map(|task| {
                match stream {
                    Stream::Stdout => task.stdout.push(line.clone()),
                    Stream::Stderr => task.stderr.push(line.clone()),
                }
                (
                    task.conversation_id,
                    task.tool.clone(),
                    task.stream_output,
                    task.pool.clone(),
                )
            })
        }) else {
            return;
        };

        events::publish(
            conversation_id,
            events::ConversationEvent::TaskUpdate {
                task_id: task_id.to_string(),
                tool: tool.clone(),
                status: "running".to_string(),
                stream: Some(stream.label().to_string()),
                latest_output: Some(line.clone()),
            },
        );

        if stream_output {
            let text = format!(
                r#"<task-output task_id="{task_id}" tool="{tool}" stream="{}">{line}</task-output>"#,
                stream.label()
            );
            let message = AnthropicMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::Text { text }],
            };
            let _ = chat::run_turn(&pool, conversation_id, message, None).await;
        }
    }

    /// Called exactly once per task, however it ends (natural completion,
    /// failure, or `cancel_task`): always both publishes a terminal
    /// `TaskUpdate` (browser) and pushes one final "task done" turn (model)
    /// carrying the result or error, regardless of `stream_output` — this is
    /// a genuine completion callback, not a low-information ping.
    async fn push_terminal_notification(pool: &PgPool, task_id: &str, status: &str, detail: &str) {
        let Some((conversation_id, tool)) = ({
            let tasks = lock_tasks();
            tasks
                .get(task_id)
                .map(|task| (task.conversation_id, task.tool.clone()))
        }) else {
            return;
        };

        events::publish(
            conversation_id,
            events::ConversationEvent::TaskUpdate {
                task_id: task_id.to_string(),
                tool: tool.clone(),
                status: status.to_string(),
                stream: None,
                latest_output: None,
            },
        );

        let text = format!(
            r#"<task-notification task_id="{task_id}" tool="{tool}">{status}: {detail}</task-notification>"#
        );
        let message = AnthropicMessage {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text }],
        };
        let _ = chat::run_turn(pool, conversation_id, message, None).await;
    }

    /// `ps`, scoped to `conversation_id` — the one tool in this suite that
    /// needs the conversation scope, since without it the model could see
    /// (and poke at) another conversation's tasks.
    pub fn snapshot_tasks(conversation_id: i64) -> Vec<TaskSummary> {
        lock_tasks()
            .iter()
            .filter(|(_, task)| task.conversation_id == conversation_id)
            .map(|(task_id, task)| TaskSummary {
                task_id: task_id.clone(),
                tool: task.tool.clone(),
                status: status_str(task.status).to_string(),
                stdout: task.stdout.clone(),
                stderr: task.stderr.clone(),
            })
            .collect()
    }

    fn list_tasks(conversation_id: i64) -> String {
        serde_json::to_string(&snapshot_tasks(conversation_id)).unwrap_or_else(|_| "[]".to_string())
    }

    /// Non-blocking `ps <pid>`.
    fn task_status_tool(input: &Value) -> Result<String, String> {
        let task_id = required_str(input, "task_id")?;
        let tasks = lock_tasks();
        let task = tasks
            .get(&task_id)
            .ok_or_else(|| format!("unknown task id: {task_id}"))?;
        Ok(status_str(task.status).to_string())
    }

    /// `tail` on a task's stdout — the accumulated log so far, in one
    /// string.
    fn task_stdout_tool(input: &Value) -> Result<String, String> {
        let task_id = required_str(input, "task_id")?;
        let tasks = lock_tasks();
        let task = tasks
            .get(&task_id)
            .ok_or_else(|| format!("unknown task id: {task_id}"))?;
        Ok(task.stdout.join("\n"))
    }

    /// `tail` on a task's stderr — same shape as `task_stdout_tool`, a
    /// separate log so diagnostics don't get mixed into a tool's real
    /// output (or vice versa).
    fn task_stderr_tool(input: &Value) -> Result<String, String> {
        let task_id = required_str(input, "task_id")?;
        let tasks = lock_tasks();
        let task = tasks
            .get(&task_id)
            .ok_or_else(|| format!("unknown task id: {task_id}"))?;
        Ok(task.stderr.join("\n"))
    }

    /// Writes a line to a running task's stdin — like piping into a
    /// process. Succeeds regardless of whether the wrapped tool actually
    /// reads its stdin (most don't; only `echo` does), same as a real pipe
    /// write succeeding even into a process that ignores stdin. Errors if
    /// the task is unknown or has already reached a terminal state (no one
    /// is ever going to read it at that point).
    fn write_task_stdin_tool(input: &Value) -> Result<String, String> {
        let task_id = required_str(input, "task_id")?;
        let data = required_str(input, "data")?;
        let tasks = lock_tasks();
        let task = tasks
            .get(&task_id)
            .ok_or_else(|| format!("unknown task id: {task_id}"))?;
        if task.status != TaskStatus::Running {
            return Err(format!(
                "task {task_id} is not running (status: {}) — nothing is reading its stdin",
                status_str(task.status)
            ));
        }
        task.stdin_tx
            .send(data)
            .map_err(|_| format!("task {task_id}'s stdin is no longer accepting input"))?;
        Ok(format!("wrote to task {task_id}'s stdin"))
    }

    /// The wrapped tool's final return value once `Finished`.
    fn task_result_tool(input: &Value) -> Result<String, String> {
        let task_id = required_str(input, "task_id")?;
        let tasks = lock_tasks();
        let task = tasks
            .get(&task_id)
            .ok_or_else(|| format!("unknown task id: {task_id}"))?;
        match task.status {
            TaskStatus::Running => {
                Err("not finished yet — use wait_task or check task_status".to_string())
            }
            TaskStatus::Finished => task.result.clone().unwrap_or_else(|| Ok(String::new())),
            TaskStatus::Failed => match &task.result {
                Some(Err(message)) => Err(message.clone()),
                _ => Err("task failed".to_string()),
            },
            TaskStatus::Cancelled => Err("task was cancelled".to_string()),
        }
    }

    /// `waitpid`/`select` with a mandatory, clamped timeout — the one tool
    /// in this suite allowed to hold the current turn open for a nontrivial,
    /// bounded amount of time. Lifted from the original 1..=10 to 1..=120 so
    /// a single `wait_task` call can meaningfully wait out `count`'s own
    /// now much wider range, instead of needing many repeated calls.
    const WAIT_TIMEOUT_RANGE: std::ops::RangeInclusive<u64> = 1..=120;

    async fn wait_task_tool(input: &Value) -> Result<String, String> {
        let task_id = required_str(input, "task_id")?;
        let timeout_seconds = required_f64(input, "timeout_seconds")? as u64;
        if !WAIT_TIMEOUT_RANGE.contains(&timeout_seconds) {
            return Err(format!(
                "timeout_seconds must be between {} and {}",
                WAIT_TIMEOUT_RANGE.start(),
                WAIT_TIMEOUT_RANGE.end()
            ));
        }

        // Create the `Notified` future *before* checking status: it's
        // guaranteed to catch a `notify_waiters()` call that happens any
        // time after this point, even one that races right between our
        // status check below and the `.await` — creating it after the
        // check would risk missing a notification that fires in between.
        let notify = {
            let tasks = lock_tasks();
            let task = tasks
                .get(&task_id)
                .ok_or_else(|| format!("unknown task id: {task_id}"))?;
            task.notify.clone()
        };
        let notified = notify.notified();

        let already_done = {
            let tasks = lock_tasks();
            let task = tasks
                .get(&task_id)
                .ok_or_else(|| format!("unknown task id: {task_id}"))?;
            task.status != TaskStatus::Running
        };

        if !already_done {
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_secs(timeout_seconds)) => {
                    return Err(format!("timed out after {timeout_seconds}s waiting for task {task_id}"));
                }
            }
        }

        task_result_tool(input)
    }

    /// `kill`: if `Running`, marks the registry entry `Cancelled` *before*
    /// calling `.abort()` — an aborted task is dropped mid-`.await` and
    /// never reaches its own "I'm done" code, so the registry write and the
    /// completion push both have to happen from the cancelling side.
    async fn cancel_task_tool(conversation_id: i64, input: &Value) -> Result<String, String> {
        let task_id = required_str(input, "task_id")?;

        enum Outcome {
            Cancelled { abort: AbortHandle, pool: PgPool },
            AlreadyDone { status: TaskStatus },
        }

        let outcome = {
            let mut tasks = lock_tasks();
            let task = tasks
                .get_mut(&task_id)
                .ok_or_else(|| format!("unknown task id: {task_id}"))?;
            if task.status == TaskStatus::Running {
                task.status = TaskStatus::Cancelled;
                Outcome::Cancelled {
                    abort: task.abort.clone(),
                    pool: task.pool.clone(),
                }
            } else {
                Outcome::AlreadyDone {
                    status: task.status,
                }
            }
        };

        match outcome {
            Outcome::AlreadyDone { status } => Ok(format!(
                "task {task_id} is already {} — nothing to cancel",
                status_str(status)
            )),
            Outcome::Cancelled { abort, pool } => {
                abort.abort();
                let _ = conversation_id; // already captured in the registry entry
                // Spawned, not awaited: `cancel_task` runs synchronously
                // inside the *calling* `run_turn`'s own tool-dispatch loop,
                // which already holds conversation_id's lock for its whole
                // duration — awaiting `push_terminal_notification` here
                // (which itself calls `chat::run_turn`) would try to
                // re-acquire that same non-reentrant lock and deadlock.
                // Detaching it lets the outer call finish and release the
                // lock first; the push then runs once that lock is free,
                // same as any other queued writer.
                let spawn_task_id = task_id.clone();
                tokio::spawn(async move {
                    push_terminal_notification(
                        &pool,
                        &spawn_task_id,
                        "cancelled",
                        "task was cancelled",
                    )
                    .await;
                });
                Ok(format!("task {task_id} cancelled"))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn test_pool() -> PgPool {
            // Never actually connected to — `execute`'s synchronous tools
            // (add/count) never touch it, and this file's own tests only
            // exercise the pool-needing paths (run_async's push mechanism)
            // through `api::chat`'s own tests, which use a real
            // `#[sqlx::test]` pool. `PgPool::connect_lazy` doesn't dial out
            // until first use, so a bogus URL is safe to construct here.
            PgPool::connect_lazy("postgres://unused:unused@localhost/unused")
                .expect("lazy pool construction never fails")
        }

        #[tokio::test]
        async fn test_add_returns_sum_as_string() {
            let pool = test_pool();
            let result = execute(
                &pool,
                1,
                "toolu_1",
                "add",
                &serde_json::json!({"a": 2, "b": 3}),
            )
            .await
            .expect("add should succeed");
            assert_eq!(result, "5");
        }

        #[tokio::test]
        async fn test_add_errors_on_missing_field() {
            let pool = test_pool();
            let result = execute(&pool, 1, "toolu_1", "add", &serde_json::json!({"a": 2})).await;
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[tokio::test]
        async fn test_count_reaches_target() {
            let pool = test_pool();
            let result = execute(
                &pool,
                1,
                "toolu_1",
                "count",
                &serde_json::json!({"target": 1, "interval_seconds": 1}),
            )
            .await
            .expect("count should succeed");
            assert_eq!(result, "Counted to 1");
        }

        #[tokio::test]
        async fn test_count_rejects_target_above_clamp() {
            let pool = test_pool();
            let result = execute(
                &pool,
                1,
                "toolu_1",
                "count",
                &serde_json::json!({"target": 2000, "interval_seconds": 1}),
            )
            .await;
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[tokio::test]
        async fn test_count_accepts_fractional_interval_seconds() {
            let pool = test_pool();
            let result = execute(
                &pool,
                1,
                "toolu_1",
                "count",
                &serde_json::json!({"target": 2, "interval_seconds": 0.05}),
            )
            .await
            .expect("count should succeed with a fractional interval");
            assert_eq!(result, "Counted to 2");
        }

        #[tokio::test]
        async fn test_unknown_tool_name_errors() {
            let pool = test_pool();
            let result = execute(&pool, 1, "toolu_1", "bogus", &serde_json::json!({})).await;
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[tokio::test]
        async fn test_run_async_rejects_unwrappable_tool() {
            let pool = test_pool();
            let result = execute(
                &pool,
                1,
                "toolu_run_async",
                "run_async",
                &serde_json::json!({"tool": "run_async", "input": {}}),
            )
            .await;
            assert!(result.is_err(), "expected an error, got {result:?}");
            assert!(
                snapshot_tasks(1).is_empty(),
                "no task should have been spawned"
            );
        }

        #[tokio::test]
        async fn test_run_async_rejects_unknown_tool() {
            let pool = test_pool();
            let result = execute(
                &pool,
                1,
                "toolu_run_async",
                "run_async",
                &serde_json::json!({"tool": "bogus", "input": {}}),
            )
            .await;
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[tokio::test]
        async fn test_run_async_rejects_wrapped_tool_input_that_violates_its_own_schema() {
            let pool = test_pool();
            let conversation_id = 150;
            // count's schema clamps target to 1..=1000 — 2000 violates it.
            // This must be caught synchronously, before any task is
            // spawned, not discovered later once the (never-going-to-
            // succeed) task actually runs and fails.
            let result = execute(
                &pool,
                conversation_id,
                "toolu_bad_count",
                "run_async",
                &serde_json::json!({"tool": "count", "input": {"target": 2000, "interval_seconds": 1}}),
            )
            .await;
            assert!(result.is_err(), "expected an error, got {result:?}");
            assert!(
                snapshot_tasks(conversation_id).is_empty(),
                "no task should have been spawned for input that fails its own schema"
            );
        }

        #[tokio::test]
        async fn test_run_async_rejects_wrapped_tool_input_missing_required_field() {
            let pool = test_pool();
            let conversation_id = 151;
            let result = execute(
                &pool,
                conversation_id,
                "toolu_bad_add",
                "run_async",
                &serde_json::json!({"tool": "add", "input": {"a": 2}}),
            )
            .await;
            assert!(result.is_err(), "expected an error, got {result:?}");
            assert!(snapshot_tasks(conversation_id).is_empty());
        }

        fn add_schema() -> Value {
            tool_definitions()
                .into_iter()
                .find(|t| t.name == "add")
                .unwrap()
                .input_schema
        }

        fn count_schema() -> Value {
            tool_definitions()
                .into_iter()
                .find(|t| t.name == "count")
                .unwrap()
                .input_schema
        }

        #[test]
        fn test_validate_against_schema_accepts_valid_input() {
            assert!(
                validate_against_schema(&serde_json::json!({"a": 1, "b": 2}), &add_schema())
                    .is_ok()
            );
            assert!(
                validate_against_schema(
                    &serde_json::json!({"target": 5, "interval_seconds": 1}),
                    &count_schema()
                )
                .is_ok()
            );
        }

        #[test]
        fn test_validate_against_schema_rejects_missing_required_field() {
            let result = validate_against_schema(&serde_json::json!({"a": 1}), &add_schema());
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[test]
        fn test_validate_against_schema_rejects_value_above_maximum() {
            let result = validate_against_schema(
                &serde_json::json!({"target": 2000, "interval_seconds": 1}),
                &count_schema(),
            );
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[test]
        fn test_validate_against_schema_rejects_value_below_minimum() {
            let result = validate_against_schema(
                &serde_json::json!({"target": 0, "interval_seconds": 1}),
                &count_schema(),
            );
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[test]
        fn test_validate_against_schema_rejects_non_integer_for_integer_field() {
            // target is declared "integer" (interval_seconds is "number",
            // so a fractional interval is legitimately valid — see
            // test_count_accepts_fractional_interval_seconds).
            let result = validate_against_schema(
                &serde_json::json!({"target": 3.5, "interval_seconds": 1}),
                &count_schema(),
            );
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[test]
        fn test_validate_against_schema_accepts_fractional_interval_seconds() {
            // The exact case from the live bug report: interval_seconds:
            // 0.1 used to be rejected on type grounds alone (declared
            // "integer"); it's "number" now, so this must pass.
            let result = validate_against_schema(
                &serde_json::json!({"target": 3, "interval_seconds": 0.1}),
                &count_schema(),
            );
            assert!(result.is_ok(), "expected success, got {result:?}");
        }

        #[test]
        fn test_validate_against_schema_rejects_wrong_json_type() {
            let result = validate_against_schema(
                &serde_json::json!({"a": "not a number", "b": 1}),
                &add_schema(),
            );
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[test]
        fn test_validate_against_schema_rejects_value_outside_enum() {
            let schema = serde_json::json!({
                "type": "object",
                "properties": {"tool": {"type": "string", "enum": ["add", "count"]}},
                "required": ["tool"]
            });
            let result = validate_against_schema(&serde_json::json!({"tool": "bogus"}), &schema);
            assert!(result.is_err(), "expected an error, got {result:?}");
        }

        #[tokio::test]
        async fn test_run_async_starts_task_visible_in_list_tasks() {
            let pool = test_pool();
            let conversation_id = 100;
            let result = execute(
                &pool,
                conversation_id,
                "toolu_add_async",
                "run_async",
                &serde_json::json!({"tool": "add", "input": {"a": 2, "b": 3}}),
            )
            .await
            .expect("run_async should succeed");
            assert!(result.contains("toolu_add_async"));

            let tasks = snapshot_tasks(conversation_id);
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].task_id, "toolu_add_async");
            assert_eq!(tasks[0].tool, "add");
        }

        #[tokio::test]
        async fn test_run_async_scopes_list_tasks_by_conversation() {
            let pool = test_pool();
            execute(
                &pool,
                200,
                "toolu_a",
                "run_async",
                &serde_json::json!({"tool": "add", "input": {"a": 1, "b": 1}}),
            )
            .await
            .expect("run_async should succeed");

            assert!(
                snapshot_tasks(201).is_empty(),
                "conversation 201 should see no tasks from 200"
            );
            assert_eq!(snapshot_tasks(200).len(), 1);
        }

        #[tokio::test]
        async fn test_wait_task_returns_result_once_finished() {
            let pool = test_pool();
            let conversation_id = 300;
            execute(
                &pool,
                conversation_id,
                "toolu_wait_add",
                "run_async",
                &serde_json::json!({"tool": "add", "input": {"a": 4, "b": 5}}),
            )
            .await
            .expect("run_async should succeed");

            let result = execute(
                &pool,
                conversation_id,
                "toolu_wait_call",
                "wait_task",
                &serde_json::json!({"task_id": "toolu_wait_add", "timeout_seconds": 5}),
            )
            .await
            .expect("wait_task should succeed");
            assert_eq!(result, "9");
        }

        #[tokio::test]
        async fn test_wait_task_times_out_on_slow_task() {
            let pool = test_pool();
            let conversation_id = 400;
            execute(
                &pool,
                conversation_id,
                "toolu_wait_count",
                "run_async",
                &serde_json::json!({"tool": "count", "input": {"target": 5, "interval_seconds": 5}}),
            )
            .await
            .expect("run_async should succeed");

            let result = execute(
                &pool,
                conversation_id,
                "toolu_wait_call",
                "wait_task",
                &serde_json::json!({"task_id": "toolu_wait_count", "timeout_seconds": 1}),
            )
            .await;
            assert!(result.is_err(), "expected a timeout error, got {result:?}");

            execute(
                &pool,
                conversation_id,
                "toolu_cancel_call",
                "cancel_task",
                &serde_json::json!({"task_id": "toolu_wait_count"}),
            )
            .await
            .expect("cancel_task should succeed");
        }

        #[tokio::test]
        async fn test_task_result_before_finished_is_an_error() {
            let pool = test_pool();
            let conversation_id = 500;
            execute(
                &pool,
                conversation_id,
                "toolu_slow_count",
                "run_async",
                &serde_json::json!({"tool": "count", "input": {"target": 5, "interval_seconds": 5}}),
            )
            .await
            .expect("run_async should succeed");

            let result = execute(
                &pool,
                conversation_id,
                "toolu_result_call",
                "task_result",
                &serde_json::json!({"task_id": "toolu_slow_count"}),
            )
            .await;
            assert!(
                result.is_err(),
                "expected an error since the task is still running, got {result:?}"
            );

            execute(
                &pool,
                conversation_id,
                "toolu_cancel_call",
                "cancel_task",
                &serde_json::json!({"task_id": "toolu_slow_count"}),
            )
            .await
            .expect("cancel_task should succeed");
        }

        #[tokio::test]
        async fn test_cancel_task_marks_cancelled_and_stops_progress() {
            let pool = test_pool();
            let conversation_id = 600;
            execute(
                &pool,
                conversation_id,
                "toolu_cancel_target",
                "run_async",
                &serde_json::json!({"tool": "count", "input": {"target": 5, "interval_seconds": 5}}),
            )
            .await
            .expect("run_async should succeed");

            let cancel_result = execute(
                &pool,
                conversation_id,
                "toolu_cancel_call",
                "cancel_task",
                &serde_json::json!({"task_id": "toolu_cancel_target"}),
            )
            .await
            .expect("cancel_task should succeed");
            assert!(cancel_result.contains("cancelled"));

            let status = execute(
                &pool,
                conversation_id,
                "toolu_status_call",
                "task_status",
                &serde_json::json!({"task_id": "toolu_cancel_target"}),
            )
            .await
            .expect("task_status should succeed");
            assert_eq!(status, "cancelled");
        }

        #[tokio::test]
        async fn test_cancel_task_on_already_finished_task_is_a_no_op_not_error() {
            let pool = test_pool();
            let conversation_id = 700;
            execute(
                &pool,
                conversation_id,
                "toolu_fast_add",
                "run_async",
                &serde_json::json!({"tool": "add", "input": {"a": 1, "b": 2}}),
            )
            .await
            .expect("run_async should succeed");

            // Give the fire-and-forget spawned task a moment to finish.
            tokio::time::sleep(Duration::from_millis(50)).await;

            let result = execute(
                &pool,
                conversation_id,
                "toolu_cancel_call",
                "cancel_task",
                &serde_json::json!({"task_id": "toolu_fast_add"}),
            )
            .await
            .expect("cancelling an already-finished task should not be an error");
            assert!(result.contains("already"));
        }

        #[tokio::test]
        async fn test_unknown_task_id_is_an_error_for_every_management_tool() {
            let pool = test_pool();
            for tool in [
                "task_status",
                "task_stdout",
                "task_stderr",
                "task_result",
                "cancel_task",
            ] {
                let result = execute(
                    &pool,
                    1,
                    "toolu_x",
                    tool,
                    &serde_json::json!({"task_id": "bogus"}),
                )
                .await;
                assert!(
                    result.is_err(),
                    "{tool} should error on an unknown task id, got {result:?}"
                );
            }
            let wait_result = execute(
                &pool,
                1,
                "toolu_x",
                "wait_task",
                &serde_json::json!({"task_id": "bogus", "timeout_seconds": 1}),
            )
            .await;
            assert!(
                wait_result.is_err(),
                "wait_task should error on an unknown task id"
            );
            let write_stdin_result = execute(
                &pool,
                1,
                "toolu_x",
                "write_task_stdin",
                &serde_json::json!({"task_id": "bogus", "data": "hello"}),
            )
            .await;
            assert!(
                write_stdin_result.is_err(),
                "write_task_stdin should error on an unknown task id"
            );
        }

        #[tokio::test]
        async fn test_echo_called_directly_returns_immediately_with_no_stdin() {
            let pool = test_pool();
            // No run_async, so no task-local task id — echo has nothing to
            // read and must not hang waiting for input that can never come.
            let result = execute(
                &pool,
                1,
                "toolu_1",
                "echo",
                &serde_json::json!({"timeout_seconds": 1}),
            )
            .await
            .expect("echo should succeed");
            assert!(result.contains("no stdin"), "got: {result}");
        }

        #[tokio::test]
        async fn test_echo_times_out_with_no_input() {
            let pool = test_pool();
            let conversation_id = 800;
            execute(
                &pool,
                conversation_id,
                "toolu_echo",
                "run_async",
                &serde_json::json!({"tool": "echo", "input": {"timeout_seconds": 1}}),
            )
            .await
            .expect("run_async should succeed");

            // Nobody ever writes to its stdin, so it should time out and
            // finish on its own within ~1s — timeout_seconds: 15 here is
            // slack for scheduler jitter under a fully-parallel test run,
            // not how long this is expected to actually take.
            let result = execute(
                &pool,
                conversation_id,
                "toolu_wait",
                "wait_task",
                &serde_json::json!({"task_id": "toolu_echo", "timeout_seconds": 15}),
            )
            .await
            .expect("wait_task should succeed");
            assert_eq!(result, "echoed 0 line(s) from stdin");
        }

        #[tokio::test]
        async fn test_write_task_stdin_round_trips_through_echo_stdout_and_stderr() {
            let pool = test_pool();
            let conversation_id = 900;
            execute(
                &pool,
                conversation_id,
                "toolu_echo_roundtrip",
                "run_async",
                &serde_json::json!({"tool": "echo", "input": {"timeout_seconds": 5}}),
            )
            .await
            .expect("run_async should succeed");

            let write_result = execute(
                &pool,
                conversation_id,
                "toolu_write",
                "write_task_stdin",
                &serde_json::json!({"task_id": "toolu_echo_roundtrip", "data": "hello task"}),
            )
            .await
            .expect("write_task_stdin should succeed");
            assert!(write_result.contains("toolu_echo_roundtrip"));

            // Give echo's loop a moment to receive and record the line —
            // it polls the channel from inside run_async's own spawned
            // task, not synchronously with this call.
            tokio::time::sleep(Duration::from_millis(100)).await;

            let stdout = execute(
                &pool,
                conversation_id,
                "toolu_stdout",
                "task_stdout",
                &serde_json::json!({"task_id": "toolu_echo_roundtrip"}),
            )
            .await
            .expect("task_stdout should succeed");
            assert_eq!(stdout, "hello task");

            let stderr = execute(
                &pool,
                conversation_id,
                "toolu_stderr",
                "task_stderr",
                &serde_json::json!({"task_id": "toolu_echo_roundtrip"}),
            )
            .await
            .expect("task_stderr should succeed");
            assert_eq!(stderr, "echo: received 10 byte(s) of input");

            // Cancel rather than wait out echo's own 5s timeout a second
            // time (it loops back to waiting for the *next* line after
            // recording this one) — the test's already proven what it
            // needs to by here, so just tidy up.
            execute(
                &pool,
                conversation_id,
                "toolu_cancel",
                "cancel_task",
                &serde_json::json!({"task_id": "toolu_echo_roundtrip"}),
            )
            .await
            .expect("cancel_task should succeed");
        }

        #[tokio::test]
        async fn test_write_task_stdin_rejects_writing_to_a_finished_task() {
            let pool = test_pool();
            let conversation_id = 901;
            execute(
                &pool,
                conversation_id,
                "toolu_fast_add_for_stdin_test",
                "run_async",
                &serde_json::json!({"tool": "add", "input": {"a": 1, "b": 2}}),
            )
            .await
            .expect("run_async should succeed");

            tokio::time::sleep(Duration::from_millis(50)).await;

            let result = execute(
                &pool,
                conversation_id,
                "toolu_write",
                "write_task_stdin",
                &serde_json::json!({"task_id": "toolu_fast_add_for_stdin_test", "data": "too late"}),
            )
            .await;
            assert!(
                result.is_err(),
                "writing to a finished task's stdin should error, got {result:?}"
            );
        }
    }
}

#[cfg(feature = "server")]
pub use server::execute;
#[cfg(feature = "server")]
pub use server::snapshot_tasks;
#[cfg(feature = "server")]
pub use server::tool_definitions;
