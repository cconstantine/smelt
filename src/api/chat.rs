use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::models::{Conversation, Message};
use crate::{anthropic, events};

#[cfg(feature = "server")]
use crate::db;
#[cfg(feature = "server")]
use crate::sandbox;

#[cfg(feature = "server")]
use sqlx::PgPool;

#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::{Arc, LazyLock, Mutex};

/// Events relayed to the browser over the `send_message` server function's
/// `ServerEvents` stream — distinct from Anthropic's own SSE event shapes,
/// which `anthropic::stream` already reduces down to plain text deltas.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    Delta {
        text: String,
    },
    Done {
        message_id: i64,
        role: String,
        content: String,
    },
    Error {
        message: String,
    },
}

#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations(db::get())
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/conversations")]
pub async fn create_conversation() -> ServerFnResult<Conversation> {
    db::create_conversation(db::get())
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    db::list_messages(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

#[delete("/api/conversations/{id}")]
pub async fn delete_conversation(id: i64) -> ServerFnResult<()> {
    // Best-effort, unconditional (unlike terminate_pod, which the model
    // calls and which is guarded) — the conversation is going away
    // regardless, so nothing about the pod matters anymore either way.
    crate::sandbox::teardown_conversation(db::get(), id).await;
    db::delete_conversation(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}

#[cfg(feature = "server")]
fn anthropic_model() -> String {
    std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "claude-opus-4-8".to_string())
}

/// On by default — set `ANTHROPIC_THINKING=0` (or `false`/`off`) to turn it
/// back off. Was briefly made opt-in after thinking broke tool use against
/// a local Ollama `gpt-oss` model (Ollama's Anthropic-compatibility shim
/// doesn't cleanly split the model's reasoning out of a tool call's
/// arguments the way real Anthropic does — see
/// `is_ollama_thinking_tool_call_corruption`) — safe to default back on
/// now that `run_turn_bounded` retries that *specific* failure without
/// thinking instead of surfacing a raw 500, rather than requiring everyone
/// to manually opt in just to get thinking against the real API.
#[cfg(feature = "server")]
fn thinking_enabled() -> bool {
    !matches!(
        std::env::var("ANTHROPIC_THINKING").as_deref(),
        Ok("0" | "false" | "off")
    )
}

/// Ollama's Anthropic-compatibility shim can fail to turn a model's raw
/// output into a valid tool call, surfacing as a flat 500 with a message
/// like `error parsing tool call: raw='...', err=...` instead of streaming
/// normally. Two different root causes share this exact shape (at least
/// for `gpt-oss`-family models): thinking's reasoning landing in the same
/// text the shim expected pure tool-call JSON in (see `thinking_enabled`'s
/// doc comment), or the model just writing invalid JSON on its own — e.g.
/// a bare `?` for a value it wasn't sure how to fill in. Real Anthropic
/// never returns this. Deliberately narrow (a distinctive, Ollama-specific
/// phrase, not just "any 500") so an unrelated upstream failure doesn't
/// silently double latency/cost by retrying pointlessly.
#[cfg(feature = "server")]
fn is_ollama_thinking_tool_call_corruption(message: &str) -> bool {
    message.contains("error parsing tool call")
}

/// Extra attempts (beyond the first) for the failure
/// `is_ollama_thinking_tool_call_corruption` recognizes — the first retry
/// also drops `thinking` (see the call site), remaining retries are plain
/// regenerations, since a local model's next sampling pass often just
/// doesn't repeat the same malformed JSON. Bounded so a call the model is
/// reliably bad at can't retry forever.
#[cfg(feature = "server")]
const TOOL_CALL_PARSE_RETRIES: usize = 2;

/// Bound on how many tool-use turns one `run_turn` call will chase before
/// giving up. Raised from the original placeholder of 5 (fine for `add`/
/// `count`, hit almost immediately by a real multi-step coding session using
/// the sandbox terminal tools) — still not a load-bearing safety limit, just
/// a backstop against looping forever. Exceeding it ends the turn with an
/// error rather than looping forever.
#[cfg(feature = "server")]
const MAX_TURNS: usize = 10_000;

/// A live `send_message` call and a background task's push-triggered
/// `run_turn` call (or two different tasks' pushes) can race for the same
/// conversation — Anthropic's strict user/assistant alternation breaks if
/// two writers persist a turn at once. Keyed by conversation id; which
/// caller acquires a given conversation's lock first when several are ready
/// is unspecified (see the plan's Open questions).
#[cfg(feature = "server")]
static CONVERSATION_LOCKS: LazyLock<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "server")]
fn conversation_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = CONVERSATION_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks
        .entry(conversation_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// The credential-requirement decision `run_turn_bounded` makes at the top
/// of every call, pulled out as a pure function over already-read env
/// values — testable without any env-var mutation, locking, or thread
/// coordination at all (mutating real `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`
/// process env vars from a test is fundamentally unsound across concurrent
/// test threads, `getenv`/`setenv` not being thread-safe at the OS level —
/// this sidesteps that entirely). At least one of `api_key`/`auth_token`
/// must be present (either is enough — `ANTHROPIC_AUTH_TOKEN` is what a
/// Hugging Face-hosted Anthropic-compatible endpoint uses instead of a
/// real Anthropic API key).
#[cfg(feature = "server")]
fn require_at_least_one_credential(api_key: &Option<String>, auth_token: &Option<String>) -> Result<(), String> {
    if api_key.is_none() && auth_token.is_none() {
        Err("neither ANTHROPIC_API_KEY nor ANTHROPIC_AUTH_TOKEN is set on the server".to_string())
    } else {
        Ok(())
    }
}

/// Runs one full tool-use round trip for `conversation_id`: persists
/// `new_message`, then loops calling the real Anthropic API — executing any
/// tool the model asks for and persisting its result — until the model
/// produces a non-`tool_use` turn or `MAX_TURNS` is exceeded. Returns every
/// message persisted along the way, in order, starting with `new_message`
/// itself. `send_message` wires a live `on_delta` into the browser's SSE
/// stream for the token-by-token typing effect; a later stage's
/// background-task push notification calls this with `on_delta = None`.
/// Returns a boxed, type-erased future rather than using plain `async fn`
/// sugar: `run_turn` and `anthropic::tools::execute` call each other
/// (`execute`'s `run_async` branch spawns a task that can call back into
/// `run_turn` to push a notification, which calls `execute` again for the
/// *next* turn's tool calls) — that mutual recursion defeats rustc's
/// `Send`-auto-trait inference for plain `async fn`s ("cannot satisfy `impl
/// Future: Send`" with no useful location). Type-erasing one edge of the
/// cycle here breaks it.
#[cfg(feature = "server")]
pub(crate) fn run_turn<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: anthropic::AnthropicMessage,
    on_delta: Option<&'a mut (dyn FnMut(&str) + Send)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'a>>
{
    run_turn_bounded(pool, conversation_id, Some(new_message), on_delta, MAX_TURNS)
}

/// Wakes `conversation_id`'s turn loop because a terminal command reached a
/// terminal state (finished or lost) — no synthetic message of its own,
/// unlike `run_turn`; just triggers the same backlog drain
/// `run_turn_bounded`'s loop already does on every iteration. A true no-op
/// if, by the time this acquires the conversation lock, nothing is actually
/// pending (e.g. another concurrent wake, or an unrelated live message,
/// already handled it) — no persisted message, no API call. This is what
/// keeps several commands finishing close together from costing one model
/// turn each. See `docs/projects/plans/terminal-exit-notify.md`.
///
/// On failure, publishes `ConversationEvent::NotificationDeliveryFailed`
/// (alongside a `tracing::warn!`) so a watching browser tab sees it live —
/// there's usually no `send_message` call in flight to relay a `ChatEvent`
/// through, since the whole point of this function is firing with no
/// request active. The underlying notification text is unaffected either
/// way: it's already durably persisted by the drain step, which runs and
/// commits *before* the API call that might fail, so this only means the
/// model hasn't been prompted with it yet, not that it's lost.
#[cfg(feature = "server")]
pub(crate) async fn wake_conversation(pool: &PgPool, conversation_id: i64) -> ServerFnResult<Vec<Message>> {
    let result = run_turn_bounded(pool, conversation_id, None, None, MAX_TURNS).await;
    if let Err(e) = &result {
        tracing::warn!(conversation_id, error = %e, "wake_conversation failed to notify the model");
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::NotificationDeliveryFailed { detail: e.to_string() },
        );
    }
    result
}

/// The real body of `run_turn`, with the turn-loop bound as a parameter —
/// exists only so `test_run_turn_errors_when_max_turns_exceeded` can prove
/// the "give up and error" behavior without actually replaying the mock
/// upstream `MAX_TURNS` (10,000) times. Every other caller goes through
/// `run_turn`, which always passes the real `MAX_TURNS`.
/// Drains any terminal commands finished (or lost) but not yet notified
/// into `history`/`persisted` as ordinary persisted `user` messages —
/// pulled out of `run_turn_bounded`'s loop so `wake_conversation` (which
/// has no synthetic message of its own to send) can trigger exactly this
/// step directly, without duplicating the notification-text logic.
#[cfg(feature = "server")]
async fn drain_unnotified_terminal_commands(
    pool: &PgPool,
    conversation_id: i64,
    history: &mut Vec<anthropic::AnthropicMessage>,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<()> {
    for command in db::unnotified_finished_terminal_commands(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?
    {
        let text = if command.status == "finished" {
            format!(
                "Terminal command {} finished: exit code {}.",
                command.command_id,
                command
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        } else {
            format!(
                "Terminal command {}'s outcome is unknown — the terminal became \
                 unreachable while it was running.",
                command.command_id
            )
        };
        let notification_content = vec![anthropic::ContentBlock::Text { text }];
        let saved = db::create_message(pool, conversation_id, "user", &notification_content)
            .await
            .map_err(ServerFnError::new)?;
        history.push(anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: notification_content,
        });
        persisted.push(saved);
        db::mark_terminal_command_notified(pool, &command.command_id)
            .await
            .map_err(ServerFnError::new)?;
    }
    Ok(())
}

#[cfg(feature = "server")]
fn run_turn_bounded<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: Option<anthropic::AnthropicMessage>,
    mut on_delta: Option<&'a mut (dyn FnMut(&str) + Send)>,
    max_turns: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'a>>
{
    Box::pin(async move {
        let lock = conversation_lock(conversation_id);
        let _guard = lock.lock().await;

        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        require_at_least_one_credential(&api_key, &auth_token).map_err(ServerFnError::new)?;

        let mut persisted = Vec::new();
        if let Some(new_message) = &new_message {
            let saved = db::create_message(
                pool,
                conversation_id,
                &new_message.role,
                &new_message.content,
            )
            .await
            .map_err(ServerFnError::new)?;
            persisted.push(saved);
        }

        let mut history: Vec<anthropic::AnthropicMessage> =
            db::list_messages(pool, conversation_id)
                .await
                .map_err(ServerFnError::new)?
                .into_iter()
                .map(|m| {
                    let content = m.blocks().map_err(ServerFnError::new)?;
                    Ok(anthropic::AnthropicMessage {
                        role: m.role,
                        content,
                    })
                })
                .collect::<ServerFnResult<Vec<_>>>()?;

        for _ in 0..max_turns {
            // Checked at the top of every loop iteration, not just once per
            // `run_turn` call — this is what gives same-turn visibility: if
            // a command finishes partway through a turn's tool-calling
            // loop, the very next iteration already sees the notification,
            // without waiting for a fresh user message. See the plan's
            // "What" and "How" (the completion-notification design).
            drain_unnotified_terminal_commands(pool, conversation_id, &mut history, &mut persisted).await?;

            // Nothing to do: `new_message` was `None` (a pure "check for a
            // backlog" wake-up, see `wake_conversation`) and the drain
            // above found nothing pending either — e.g. another concurrent
            // wake, or an unrelated live message, already handled it. No
            // message to send and no reason to call the model, so this
            // returns before ever building a request. Only possible on the
            // very first iteration: every later one already has a real
            // tool_use turn's results to send regardless.
            if new_message.is_none() && persisted.is_empty() {
                return Ok(persisted);
            }

            let mut request = anthropic::CreateMessageRequest {
                model: anthropic_model(),
                // Raised alongside `thinking`: adaptive thinking shares
                // this budget with the actual reply, and 4096 left no
                // headroom for both once thinking turned on.
                max_tokens: 16_384,
                system: None,
                messages: history.clone(),
                stream: true,
                tools: anthropic::tools::tool_definitions(pool).await,
                thinking: thinking_enabled().then_some(anthropic::ThinkingConfig::Adaptive),
            };

            let mut relay = |delta: &str| {
                if let Some(cb) = on_delta.as_deref_mut() {
                    cb(delta);
                }
            };

            // See `is_ollama_thinking_tool_call_corruption` — not always
            // caused by thinking specifically (a local model can just
            // flub a tool call's JSON on its own, e.g. writing a bare `?`
            // for a value it wasn't sure about), so the mitigation is two
            // parts: drop `thinking` once (a plausible contributing
            // factor, and free to try), then fall back to plain retries —
            // regenerating is often enough on its own since sampling
            // varies run to run. Bounded so a model that's reliably bad at
            // one particular call can't loop forever.
            let mut turn = None;
            let mut last_err = String::new();
            for attempt in 0..=TOOL_CALL_PARSE_RETRIES {
                match anthropic::stream::stream_anthropic_message(api_key.as_deref(), auth_token.as_deref(), &request, &mut relay).await {
                    Ok(t) => {
                        turn = Some(t);
                        break;
                    }
                    Err(e) if attempt < TOOL_CALL_PARSE_RETRIES && is_ollama_thinking_tool_call_corruption(&e) => {
                        request.thinking = None;
                        last_err = e;
                    }
                    Err(e) => return Err(ServerFnError::new(e)),
                }
            }
            let turn = turn.ok_or_else(|| ServerFnError::new(last_err))?;

            let saved = db::create_message(pool, conversation_id, "assistant", &turn.content)
                .await
                .map_err(ServerFnError::new)?;
            history.push(anthropic::AnthropicMessage {
                role: "assistant".to_string(),
                content: turn.content.clone(),
            });
            persisted.push(saved);

            if turn.stop_reason != "tool_use" {
                crate::events::publish(
                    conversation_id,
                    crate::events::ConversationEvent::MessagesAppended(persisted.clone()),
                );
                return Ok(persisted);
            }

            let mut result_blocks = Vec::new();
            for block in &turn.content {
                if let anthropic::ContentBlock::ToolUse { id, name, input } = block {
                    let result =
                        anthropic::tools::execute(pool, conversation_id, id, name, input).await;
                    let (content, is_error) = match result {
                        Ok(output) => (output, None),
                        Err(message) => (message, Some(true)),
                    };
                    result_blocks.push(anthropic::ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content,
                        is_error,
                    });
                }
            }

            let saved = db::create_message(pool, conversation_id, "user", &result_blocks)
                .await
                .map_err(ServerFnError::new)?;
            history.push(anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: result_blocks,
            });
            persisted.push(saved);
        }

        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::MessagesAppended(persisted),
        );
        Err(ServerFnError::new(format!(
            "tool-use loop exceeded {max_turns} turns without reaching a final reply"
        )))
    })
}

#[post("/api/conversations/{id}/messages")]
pub async fn send_message(id: i64, content: String) -> ServerFnResult<ServerEvents<ChatEvent>> {
    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text: content }],
    };

    Ok(ServerEvents::new(move |mut tx| async move {
        let mut on_delta = |delta: &str| {
            // `on_delta` is a plain sync `FnMut`, but `SseTx::send` is only
            // `async` for API symmetry — it wraps a synchronous
            // `unbounded_send` with no `.await` inside, so send through the
            // underlying channel directly here. This keeps delta ordering
            // exact (a spawned task per delta could interleave out of order).
            if let Ok(event) = axum::response::sse::Event::default().json_data(ChatEvent::Delta {
                text: delta.to_string(),
            }) {
                let _ = tx.unbounded_send(event);
            }
        };

        match run_turn(db::get(), id, new_message, Some(&mut on_delta)).await {
            Ok(messages) => {
                // messages[0] is the caller's own new user message — already
                // shown optimistically by the frontend the instant it was
                // sent, so only the turns `run_turn` produced afterward
                // (assistant replies, tool results) are new to relay.
                for message in messages.into_iter().skip(1) {
                    let _ = tx
                        .send(ChatEvent::Done {
                            message_id: message.id,
                            role: message.role,
                            content: message.content,
                        })
                        .await;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }))
}

/// Thin wrapper around `anthropic::tools::snapshot_tasks` for the browser —
/// a one-shot pull, not a subscription. Used both for the initial task-panel
/// load and for the reconciliation pull `subscribe_conversation_events`'s
/// caller does on connect/reconnect (a `broadcast` channel has no replay).
#[get("/api/conversations/{id}/tasks")]
pub async fn get_tasks(id: i64) -> ServerFnResult<Vec<anthropic::tools::TaskSummary>> {
    Ok(anthropic::tools::snapshot_tasks(id))
}

/// One line of a command's output, in the order it actually happened —
/// `stdout`/`stderr` fetched and capped independently (see
/// `fetch_command_summary`) but merged back into one true chronological
/// sequence here, rather than the panel showing "all stdout, then all
/// stderr" the way two separate fields would.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxOutputLine {
    pub stream: String,
    pub data: String,
}

/// One terminal's current/most recent command, hydrated for the sandbox
/// panel's initial scrollback — see `docs/projects/completed/20260815-sandbox-visibility.md`.
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
async fn fetch_command_summary(pool: &PgPool, command: &db::TerminalCommand) -> Option<SandboxCommandSummary> {
    let status = db::terminal_command_status(pool, &command.command_id).await.ok()??;
    let stdout_offset = (status.stdout_lines - SNAPSHOT_TAIL_LINES).max(0);
    let stderr_offset = (status.stderr_lines - SNAPSHOT_TAIL_LINES).max(0);
    let stdout = db::read_terminal_output(pool, &command.command_id, &["stdout"], stdout_offset, SNAPSHOT_TAIL_LINES)
        .await
        .unwrap_or_default();
    let stderr = db::read_terminal_output(pool, &command.command_id, &["stderr"], stderr_offset, SNAPSHOT_TAIL_LINES)
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
            .map(|line| SandboxOutputLine { stream: line.stream, data: line.data })
            .collect(),
    })
}

#[cfg(feature = "server")]
async fn fetch_terminal_command_history(pool: &PgPool, terminal_id: i64) -> Vec<SandboxCommandSummary> {
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
/// browser — a one-shot pull, not a subscription, same shape as `get_tasks`.
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
    let pods = sandbox::list_pods(pool, id).await.map_err(ServerFnError::new)?;
    for pod in &pods {
        sandbox::try_reconnect(pool, pod.pod_id).await;
    }
    let terminals = sandbox::list_terminals(pool, id).await.map_err(ServerFnError::new)?;

    let mut by_pod: HashMap<i64, Vec<SandboxTerminalSummary>> = HashMap::new();
    for terminal in terminals {
        let commands = fetch_terminal_command_history(pool, terminal.terminal_id).await;
        by_pod.entry(terminal.pod_id).or_default().push(SandboxTerminalSummary {
            terminal_id: terminal.terminal_id,
            pod_id: terminal.pod_id,
            status: terminal.status,
            commands,
        });
    }

    let pods = pods
        .into_iter()
        .map(|pod| SandboxPodSummary {
            pod_id: pod.pod_id,
            status: pod.status,
            terminals: by_pod.remove(&pod.pod_id).unwrap_or_default(),
        })
        .collect();

    Ok(SandboxSnapshot { pods })
}

/// A dedicated, always-open per-conversation event stream — independent of
/// any particular `send_message` call, since task activity (a tick, a
/// finish) or another writer's pushed turn can happen with no request in
/// flight at all. The frontend opens this once per viewed conversation and
/// keeps it open for as long as that conversation is selected; see
/// `docs/architecture.md` for why this needs its own stream rather than
/// reusing `send_message`'s.
#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(
    id: i64,
) -> ServerFnResult<ServerEvents<events::ConversationEvent>> {
    Ok(ServerEvents::new(move |mut tx| async move {
        let mut rx = events::subscribe(id);
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = tx.send(event).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // A subscriber that fell behind just misses some
                    // ephemeral `TaskUpdate`s — the frontend's one-shot
                    // `get_messages`/`get_tasks` reconciliation pull on
                    // connect covers the durable state regardless.
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Spins up a mock Anthropic upstream that returns `bodies` in order (one
    /// per request, clamped to the last body once exhausted) and points
    /// `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY` (both process-global) at it.
    /// Callers must hold `anthropic::test_support::lock_anthropic_base_url`
    /// for the duration, same as `anthropic::stream`'s own mock-upstream
    /// tests.
    async fn start_mock_upstream(bodies: Vec<String>) -> Arc<AtomicUsize> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let bodies = Arc::new(bodies);
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_route = counter.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let bodies = bodies.clone();
                let counter = counter_for_route.clone();
                async move {
                    let i = counter.fetch_add(1, Ordering::SeqCst);
                    let body = bodies[i.min(bodies.len() - 1)].clone();
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
        counter
    }

    fn sse_body(events: &[(&str, &str)]) -> String {
        events
            .iter()
            .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
            .collect()
    }

    /// Like `start_mock_upstream`, but the first `fail_count` requests get
    /// back a flat HTTP 500 with Ollama's real "error parsing tool call"
    /// body — the exact shape `is_ollama_thinking_tool_call_corruption` is
    /// meant to recognize — and every request after that gets
    /// `success_body` (a normal 200 SSE stream), if any. `success_body:
    /// None` means every request fails, for testing the give-up path once
    /// `TOOL_CALL_PARSE_RETRIES` is exhausted.
    async fn start_mock_upstream_failing_n_times(fail_count: usize, success_body: Option<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let counter = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let counter = counter.clone();
                let success_body = success_body.clone();
                async move {
                    let i = counter.fetch_add(1, Ordering::SeqCst);
                    if i < fail_count || success_body.is_none() {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            format!(
                                r#"{{"type":"error","error":{{"type":"api_error","message":"error parsing tool call: raw='attempt {i}' err=invalid character '?' after object key:value pair"}},"request_id":"req_test"}}"#
                            ),
                        )
                            .into_response()
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            success_body.expect("checked above"),
                        )
                            .into_response()
                    }
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}"));
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
    }

    /// `ANTHROPIC_THINKING` is process-global like `ANTHROPIC_BASE_URL`, but
    /// unlike it, no other test reads `thinking_enabled()`'s result, so
    /// nothing else can fail if a concurrently-running test transiently
    /// observes this one's value — restoring it afterward (rather than a
    /// dedicated lock) is enough.
    #[test]
    fn test_thinking_enabled_defaults_to_on_and_recognizes_opt_out_values() {
        // `ANTHROPIC_THINKING` is process-global, same as `ANTHROPIC_BASE_URL`
        // — reusing that lock (rather than a dedicated one) keeps this
        // mutually exclusive with `test_run_turn_retries_without_thinking_...`,
        // which needs the *default* (on) to actually exercise the retry path.
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let original = std::env::var("ANTHROPIC_THINKING").ok();

        unsafe { std::env::remove_var("ANTHROPIC_THINKING") };
        assert!(thinking_enabled(), "should default to on — see the doc comment for why");

        for value in ["0", "false", "off"] {
            unsafe { std::env::set_var("ANTHROPIC_THINKING", value) };
            assert!(!thinking_enabled(), "{value:?} should disable thinking");
        }

        for value in ["1", "true", "on", "nonsense"] {
            unsafe { std::env::set_var("ANTHROPIC_THINKING", value) };
            assert!(thinking_enabled(), "{value:?} should not disable thinking");
        }

        match original {
            Some(value) => unsafe { std::env::set_var("ANTHROPIC_THINKING", value) },
            None => unsafe { std::env::remove_var("ANTHROPIC_THINKING") },
        }
    }

    #[test]
    fn test_is_ollama_thinking_tool_call_corruption_matches_the_known_error_shape() {
        let real_error = r#"Anthropic API error 500 Internal Server Error: {"type":"error","error":{"type":"api_error","message":"error parsing tool call: raw='...' err=invalid character 'T' looking for beginning of value"},"request_id":"req_123"}"#;
        assert!(is_ollama_thinking_tool_call_corruption(real_error));
    }

    #[test]
    fn test_is_ollama_thinking_tool_call_corruption_does_not_match_unrelated_errors() {
        assert!(!is_ollama_thinking_tool_call_corruption(
            "Anthropic API error 529 Overloaded: the server is overloaded"
        ));
        assert!(!is_ollama_thinking_tool_call_corruption(
            "timed out waiting for Anthropic to respond"
        ));
    }

    /// `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` are process-global, same
    /// as `ANTHROPIC_BASE_URL` — restoring both afterward (rather than a
    /// dedicated lock) is enough, same reasoning
    /// `test_thinking_enabled_defaults_to_on_and_recognizes_opt_out_values`
    /// already applies to `ANTHROPIC_THINKING`. Callers must still hold
    /// `lock_anthropic_base_url` for the duration, since this also touches
    /// `ANTHROPIC_BASE_URL`-adjacent test infrastructure other tests share.
    struct CredentialEnvGuard {
        original_api_key: Option<String>,
        original_auth_token: Option<String>,
    }

    impl CredentialEnvGuard {
        fn capture() -> Self {
            Self {
                original_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
                original_auth_token: std::env::var("ANTHROPIC_AUTH_TOKEN").ok(),
            }
        }
    }

    impl Drop for CredentialEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original_api_key {
                    Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                    None => std::env::remove_var("ANTHROPIC_API_KEY"),
                }
                match &self.original_auth_token {
                    Some(v) => std::env::set_var("ANTHROPIC_AUTH_TOKEN", v),
                    None => std::env::remove_var("ANTHROPIC_AUTH_TOKEN"),
                }
            }
        }
    }

    #[test]
    fn test_require_at_least_one_credential_errors_when_both_are_missing() {
        let err = require_at_least_one_credential(&None, &None)
            .expect_err("should error when neither credential is set");
        assert!(
            err.contains("ANTHROPIC_API_KEY") && err.contains("ANTHROPIC_AUTH_TOKEN"),
            "expected the error to name both env vars, got: {err}"
        );
    }

    #[test]
    fn test_require_at_least_one_credential_allows_api_key_only() {
        require_at_least_one_credential(&Some("sk-ant-...".to_string()), &None)
            .expect("an API key alone should be sufficient");
    }

    #[test]
    fn test_require_at_least_one_credential_allows_auth_token_only() {
        require_at_least_one_credential(&None, &Some("hf-token".to_string()))
            .expect("an auth token alone should be sufficient — e.g. a Hugging Face-hosted endpoint");
    }

    #[test]
    fn test_require_at_least_one_credential_allows_both_present() {
        require_at_least_one_credential(&Some("sk-ant-...".to_string()), &Some("hf-token".to_string()))
            .expect("both being present should still be fine");
    }

    #[sqlx::test]
    async fn test_run_turn_succeeds_with_only_auth_token_set_no_api_key(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let _env_guard = CredentialEnvGuard::capture();
        let conversation = db::create_conversation(&pool).await.expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::set_var("ANTHROPIC_AUTH_TOKEN", "hf-token");
        }

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text { text: "hello".to_string() }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed using only ANTHROPIC_AUTH_TOKEN, with no ANTHROPIC_API_KEY set");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, "assistant");
    }

    /// A finished, not-yet-notified terminal command — the state
    /// `wake_conversation` is meant to react to. Mirrors `db.rs`'s own
    /// `test_terminal` helper shape.
    async fn unnotified_finished_command(pool: &PgPool, conversation_id: i64, command_id: &str) {
        let pod = db::create_sandbox_pod(pool, conversation_id).await.expect("create sandbox pod");
        let terminal = db::create_sandbox_terminal(pool, pod.id).await.expect("create sandbox terminal");
        db::create_terminal_command(pool, conversation_id, terminal.id, command_id, "echo hi")
            .await
            .expect("create terminal command");
        db::mark_terminal_command_finished(pool, command_id, 0)
            .await
            .expect("mark terminal command finished");
    }

    #[sqlx::test]
    async fn test_wake_conversation_is_a_noop_when_nothing_is_pending(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let counter = start_mock_upstream(vec!["unused".to_string()]).await;

        let result = wake_conversation(&pool, conversation.id)
            .await
            .expect("wake_conversation should succeed even with nothing pending");
        assert!(result.is_empty(), "expected no persisted messages, got {result:?}");
        assert_eq!(counter.load(Ordering::SeqCst), 0, "nothing pending should mean no API call at all");

        let messages = db::list_messages(&pool, conversation.id).await.expect("list messages");
        assert!(messages.is_empty(), "no message should be persisted when nothing is pending");
    }

    #[sqlx::test]
    async fn test_wake_conversation_drains_a_pending_command_and_completes_a_turn(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let messages = wake_conversation(&pool, conversation.id)
            .await
            .expect("wake_conversation should succeed");

        assert_eq!(messages.len(), 2, "expected the notification plus the assistant's reply, got {messages:?}");
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            messages[0].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Terminal command cmd-1 finished: exit code 0.".to_string()
            }]
        );
        assert_eq!(messages[1].role, "assistant");

        let remaining = db::unnotified_finished_terminal_commands(&pool, conversation.id)
            .await
            .expect("query unnotified commands");
        assert!(remaining.is_empty(), "the command should now be marked notified");
    }

    #[sqlx::test]
    async fn test_wake_conversation_second_call_is_a_noop_once_the_first_drained_everything(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let counter = start_mock_upstream(vec![body]).await;

        let first = wake_conversation(&pool, conversation.id).await.expect("first wake should succeed");
        assert_eq!(first.len(), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 1, "first wake should make exactly one API call");

        // Simulates a second, near-simultaneous exit event's own detached
        // wake_conversation call — nothing should be left to drain, so this
        // must not persist another message or make another API call.
        let second = wake_conversation(&pool, conversation.id).await.expect("second wake should succeed");
        assert!(second.is_empty(), "second wake should find nothing left to drain, got {second:?}");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "second wake should not make another API call");
    }

    #[sqlx::test]
    async fn test_wake_conversation_publishes_notification_delivery_failed_on_error(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

        // `fail_count` is irrelevant when `success_body` is `None` — every
        // request fails regardless (see the helper's doc comment).
        start_mock_upstream_failing_n_times(0, None).await;

        let mut rx = events::subscribe(conversation.id);

        let result = wake_conversation(&pool, conversation.id).await;
        assert!(result.is_err(), "expected wake_conversation to surface the underlying failure, got {result:?}");

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("should not time out waiting for the event")
            .expect("event channel should not close");
        assert!(
            matches!(event, events::ConversationEvent::NotificationDeliveryFailed { .. }),
            "expected NotificationDeliveryFailed, got {event:?}"
        );

        // The notification text itself is durably persisted regardless —
        // the drain step commits before the API call that failed.
        let messages = db::list_messages(&pool, conversation.id).await.expect("list messages");
        assert_eq!(messages.len(), 1, "the notification message should still be persisted, got {messages:?}");
        assert_eq!(messages[0].role, "user");
    }

    #[sqlx::test]
    async fn test_run_turn_persists_user_and_assistant_messages_for_text_only_reply(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            messages[0].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "hello".to_string()
            }]
        );
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Hi!".to_string()
            }]
        );
    }

    /// Regression test for conversation 43's real "500 error parsing tool
    /// call" incident: with thinking on (the default) and pointed at a
    /// mock upstream that fails the *first* request with Ollama's exact
    /// error shape, `run_turn` should retry without thinking and still
    /// complete — not surface the 500 to the caller.
    #[sqlx::test]
    async fn test_run_turn_retries_without_thinking_after_ollama_tool_call_corruption(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let success_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream_failing_n_times(1, Some(success_body)).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "ping".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should recover from the failed first attempt and succeed");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "pong".to_string()
            }],
            "the retried (thinking-free) attempt's reply should be what actually got persisted"
        );
    }

    /// Dropping `thinking` doesn't help every case (a local model can flub
    /// a tool call's JSON on its own — see `is_ollama_thinking_tool_call_corruption`'s
    /// doc comment) — this fails *twice*, past the thinking-drop, and
    /// relies on `TOOL_CALL_PARSE_RETRIES` allowing one further plain
    /// regeneration to still recover.
    #[sqlx::test]
    async fn test_run_turn_recovers_after_two_ollama_tool_call_failures(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let success_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"third time's the charm"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream_failing_n_times(2, Some(success_body)).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "ping".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should recover after exhausting the thinking-drop and one plain retry");

        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "third time's the charm".to_string()
            }]
        );
    }

    /// Once `TOOL_CALL_PARSE_RETRIES` is exhausted, `run_turn` gives up and
    /// surfaces the error rather than retrying forever against a call the
    /// model is reliably bad at.
    #[sqlx::test]
    async fn test_run_turn_gives_up_after_exhausting_tool_call_parse_retries(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // `fail_count` is irrelevant when `success_body` is `None` — every
        // request fails regardless (see the helper's doc comment).
        start_mock_upstream_failing_n_times(0, None).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "ping".to_string(),
            }],
        };

        let err = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect_err("should give up and surface the error once retries are exhausted");
        assert!(
            err.to_string().contains("error parsing tool call"),
            "got {err}"
        );
    }

    #[sqlx::test]
    async fn test_run_turn_executes_tool_and_persists_full_round_trip(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let tool_use_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"add","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":2,\"b\":3}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let final_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sum is 5"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![tool_use_body, final_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "please add 2 and 3".to_string(),
            }],
        };

        let messages = run_turn(&pool, conversation.id, new_message, None)
            .await
            .expect("run_turn should succeed");

        assert_eq!(
            messages.len(),
            4,
            "expected user, tool_use, tool_result, final assistant"
        );
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "add".to_string(),
                input: serde_json::json!({"a": 2, "b": 3}),
            }]
        );
        assert_eq!(messages[2].role, "user");
        assert_eq!(
            messages[2].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolResult {
                tool_use_id: "toolu_01".to_string(),
                content: "5".to_string(),
                is_error: None,
            }]
        );
        assert_eq!(messages[3].role, "assistant");
        assert_eq!(
            messages[3].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::Text {
                text: "Sum is 5".to_string()
            }]
        );
    }

    #[sqlx::test]
    async fn test_run_turn_errors_when_max_turns_exceeded(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // Always responds with a tool_use turn calling `add` (a fast, valid
        // call), so the loop never reaches a final reply and must give up
        // after MAX_TURNS rather than looping forever.
        let tool_use_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"add","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1,\"b\":1}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![tool_use_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "loop forever".to_string(),
            }],
        };

        // Goes through run_turn_bounded directly with a small bound rather
        // than run_turn (which would replay the mock upstream the real
        // MAX_TURNS — 10,000 — times just to prove the same "give up and
        // error" behavior).
        let result = run_turn_bounded(&pool, conversation.id, Some(new_message), None, 3).await;
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    #[test]
    fn test_conversation_lock_is_shared_per_conversation_id_only() {
        let a1 = conversation_lock(9001);
        let a2 = conversation_lock(9001);
        let b = conversation_lock(9002);
        assert!(
            Arc::ptr_eq(&a1, &a2),
            "same conversation id should share one lock"
        );
        assert!(
            !Arc::ptr_eq(&a1, &b),
            "different conversation ids should get different locks"
        );
    }

    #[sqlx::test]
    async fn test_run_turn_serializes_concurrent_calls_for_the_same_conversation(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![body]).await;

        let conversation_id = conversation.id;
        let pool_a = pool.clone();
        let pool_b = pool.clone();

        let task_a = tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text {
                    text: "first".to_string(),
                }],
            };
            run_turn(&pool_a, conversation_id, message, None).await
        });
        let task_b = tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text {
                    text: "second".to_string(),
                }],
            };
            run_turn(&pool_b, conversation_id, message, None).await
        });

        let (result_a, result_b) = tokio::join!(task_a, task_b);
        result_a
            .expect("task a should not panic")
            .expect("run_turn a should succeed");
        result_b
            .expect("task b should not panic")
            .expect("run_turn b should succeed");

        let all = db::list_messages(&pool, conversation_id)
            .await
            .expect("list messages");
        assert_eq!(all.len(), 4);
        let roles: Vec<&str> = all.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "assistant"],
            "the per-conversation lock should serialize the two calls into two complete \
             (user, assistant) pairs, never interleaved"
        );
    }

    /// Regression test for a deadlock: `cancel_task` runs synchronously
    /// inside `run_turn`'s own tool-dispatch loop (same as `add`/`count`/
    /// any other tool), which already holds `conversation_id`'s lock for
    /// its entire duration. `cancel_task` also pushes a cancellation
    /// notification via `chat::run_turn` — if that push were awaited
    /// in-line rather than detached (`tokio::spawn`), it would try to
    /// re-acquire the same non-reentrant lock the outer call is still
    /// holding and hang forever. Wrapped in a timeout so a regression
    /// fails loudly instead of hanging the test suite.
    #[sqlx::test]
    async fn test_run_turn_does_not_deadlock_when_model_calls_cancel_task(pool: PgPool) {
        let _guard = anthropic::test_support::lock_anthropic_base_url();
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        // Seed a running task directly (bypassing the model) for cancel_task
        // to act on.
        let start_result = anthropic::tools::execute(
            &pool,
            conversation.id,
            "toolu_seed_task",
            "run_async",
            &serde_json::json!({"tool": "count", "input": {"target": 5, "interval_seconds": 5}}),
        )
        .await
        .expect("seeding the background task should succeed");
        assert!(start_result.contains("toolu_seed_task"));

        let cancel_turn_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_cancel","name":"cancel_task","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"task_id\":\"toolu_seed_task\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        let final_body = sse_body(&[
            ("message_start", r#"{"type":"message_start"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cancelled it"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        start_mock_upstream(vec![cancel_turn_body, final_body]).await;

        let new_message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "cancel that task".to_string(),
            }],
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_turn(&pool, conversation.id, new_message, None),
        )
        .await
        .expect("run_turn should complete well within the timeout, not deadlock")
        .expect("run_turn should succeed");

        assert_eq!(
            result.len(),
            4,
            "expected user, tool_use(cancel_task), tool_result, final assistant"
        );
        assert_eq!(
            result[2].blocks().expect("valid blocks"),
            vec![anthropic::ContentBlock::ToolResult {
                tool_use_id: "toolu_cancel".to_string(),
                content: "task toolu_seed_task cancelled".to_string(),
                is_error: None,
            }]
        );
    }

    /// Regression test: `run_async` and the task-management suite were
    /// fully implemented and unit-tested in `anthropic::tools` before
    /// anyone noticed `tool_definitions()` never listed them — the model
    /// had no way to know they existed until a live test asked it to use
    /// `run_async` and it correctly said no such tool was available. This
    /// pins the two lists together so a newly dispatchable tool can't be
    /// implemented without also being offered to the model.
    ///
    /// Scoped to `native_tool_definitions()` deliberately — this test is
    /// about smelt's own static dispatch names, not MCP servers (which are
    /// dynamic/external and have no fixed name list to pin against).
    #[test]
    fn test_tool_definitions_covers_every_dispatchable_tool_name() {
        let defined: std::collections::BTreeSet<String> = anthropic::tools::native_tool_definitions()
            .into_iter()
            .map(|t| t.name)
            .collect();
        let dispatchable: std::collections::BTreeSet<&str> = [
            "add",
            "count",
            "echo",
            "run_async",
            "list_tasks",
            "task_status",
            "task_stdout",
            "task_stderr",
            "task_result",
            "wait_task",
            "cancel_task",
            "write_task_stdin",
            "create_pod",
            "terminate_pod",
            "list_pods",
            "create_terminal",
            "terminate_terminal",
            "list_terminals",
            "run_terminal_command",
            "send_signal",
            "terminal_command_status",
            "read_terminal_output",
            "list_commands",
        ]
        .into_iter()
        .collect();

        let missing: Vec<_> = dispatchable
            .iter()
            .filter(|name| !defined.contains(**name))
            .collect();
        assert!(
            missing.is_empty(),
            "native_tool_definitions() is missing: {missing:?}"
        );
    }
}
