# API — Dioxus Server Functions

There is no hand-rolled Axum API layer and no hand-written browser fetch client. Every endpoint is a plain async function in `src/api/chat.rs` decorated with `#[get(...)]` or `#[post(...)]` (from `dioxus::prelude`, available under the `fullstack` feature). The macro generates two different bodies for the same function signature:

- **Server build** (`feature = "server"`): registers a real Axum route (via `inventory::submit!`) and runs your actual function body.
- **Web build** (`feature = "web"`): replaces the body with a transparent HTTP call to that route, JSON-encoding the arguments and decoding the response.

So a frontend component calls `get_conversations()` exactly like calling a local async function — there is no separate `frontend/api.rs` fetch-helper file to keep in sync with the server routes.

```rust
#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations().await.map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    db::list_messages(id).await.map_err(ServerFnError::new)
}
```

Path parameters (`{id}`) map directly onto a same-named function argument. `ServerFnResult<T>` is `Result<T, ServerFnError>` — return it from every server function; the macro asserts on this at compile time.

## Streaming: `send_message`

`send_message` is a `ServerFnResult<ServerEvents<ChatEvent>>` — Dioxus fullstack's native SSE payload type. Since tool-use landed, it's backed by a *loop* (`api::chat::run_turn`, capped at `MAX_TURNS`), not one Anthropic call: the request now carries a `tools: Vec<ToolDefinition>` list (see `anthropic::types`), and whenever a turn's `stop_reason` is `"tool_use"`, `run_turn` executes each tool (`anthropic::tools::execute`), persists the `ToolResult` turn, and loops again. Exceeding `MAX_TURNS` ends the turn with `ChatEvent::Error` rather than looping forever.

```rust
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    Delta { text: String },
    Done { message_id: i64, role: String, content: String },
    Error { message: String },
}

#[post("/api/conversations/{id}/messages")]
pub async fn send_message(id: i64, content: String) -> ServerFnResult<ServerEvents<ChatEvent>> {
    let new_message = AnthropicMessage { role: "user".to_string(), content: vec![ContentBlock::Text { text: content }] };

    Ok(ServerEvents::new(move |mut tx| async move {
        // on_delta is wired straight into a synchronous unbounded_send (not
        // the async SseTx::send wrapper) so delta ordering stays exact.
        match run_turn(db::get(), id, new_message, Some(&mut on_delta)).await {
            Ok(messages) => {
                // messages[0] is the caller's own message, already shown
                // optimistically by the frontend — only relay what run_turn
                // produced afterward.
                for message in messages.into_iter().skip(1) {
                    let _ = tx.send(ChatEvent::Done { message_id: message.id, role: message.role, content: message.content }).await;
                }
            }
            Err(e) => { let _ = tx.send(ChatEvent::Error { message: e.to_string() }).await; }
        }
    }))
}
```

Requests carry `thinking: {"type": "adaptive"}` by default (`ANTHROPIC_THINKING=0` to turn it off — see [setup.md](setup.md)), so an assistant turn's `content` can start with a `ContentBlock::Thinking { thinking, signature }` block ahead of any `Text`/`ToolUse` blocks — `run_turn` persists and replays it exactly like any other block, uninterpreted; the frontend renders it as a collapsed-by-default `<details>` (see `frontend/pages/chat.rs`'s `render_block_element`).

If a request fails with Ollama's specific "error parsing tool call" 500 (its Anthropic-compat shim, at least for `gpt-oss` models, doesn't always turn a model's raw output into valid tool-call JSON — either because thinking's reasoning landed in the same text as the call, or the model just wrote invalid JSON on its own), `run_turn_bounded` retries: first with `thinking` dropped, then up to `TOOL_CALL_PARSE_RETRIES` further plain regenerations, since a local model's next sampling pass often doesn't repeat the same malformed output. Gives up and surfaces the error once that's exhausted. Any other failure (a real rate limit, a genuinely different error) propagates immediately, with no retry. See `is_ollama_thinking_tool_call_corruption`.

**`ChatEvent::Done` can fire more than once per `send_message` call** — once for each message `run_turn` persisted in that turn loop (an assistant `ToolUse` turn, the `ToolResult` turn, a final assistant reply — however many rounds the tool-use loop took). The frontend's existing "push each `Done` onto `messages`" loop handles this unchanged, since it was already written to handle an arbitrary number of `Done`s.

On the client, calling `send_message(id, content).await` returns the `ServerEvents<ChatEvent>` immediately (as soon as the connection opens), and the caller iterates it as events arrive — same shape as before, just potentially more `Done`s:

```rust
let mut events = send_message(id, content).await?;
while let Some(event) = events.recv().await {
    match event? {
        ChatEvent::Delta { text } => streaming_text.write().push_str(&text),
        ChatEvent::Done { message_id, role, content } => { /* append the persisted message */ }
        ChatEvent::Error { message } => { /* show it */ }
    }
}
```

Because sending a message and opening its event stream are the same call, there's no POST-then-GET race to worry about — a hazard a two-endpoint hand-rolled SSE design would need to guard against explicitly.

**Some rows are no longer guaranteed to be persisted by the request that "sent" them.** A `run_async` tool call returns immediately, but the task it started can later push its own turns onto the conversation via `run_turn` from a background task — with no `send_message` call in flight at all. Those rows are stored exactly like any other turn (see [models.md](models.md)), just written outside the request/response cycle a naive reading of this API might assume. See "Live conversation events" below for how a browser tab learns about them.

## Live conversation events

Three server functions exist purely to support live-updating panels (the background-tasks panel via `run_async`, and the sandbox panel — see `docs/projects/completed/20260815-sandbox-visibility.md`) and turns pushed from outside a request:

```rust
#[get("/api/conversations/{id}/tasks")]
pub async fn get_tasks(id: i64) -> ServerFnResult<Vec<anthropic::tools::TaskSummary>>;

#[get("/api/conversations/{id}/sandbox")]
pub async fn get_sandbox_state(id: i64) -> ServerFnResult<SandboxSnapshot>;

#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(id: i64) -> ServerFnResult<ServerEvents<ConversationEvent>>;
```

`get_tasks` is a thin, one-shot wrapper around `anthropic::tools::snapshot_tasks` — every task started via `run_async` in that conversation, with its current status. `get_sandbox_state` is the same shape for the sandbox panel: every pod and terminal currently live in the conversation, each terminal hydrated with its `HISTORY_LIMIT` most recent commands (oldest first), each with the last 200 lines per stream, merged back into one true chronological `output` (stdout and stderr are fetched/capped independently so one stream can't crowd the other out of the window, then re-sorted by `seq` — see `fetch_command_summary` — so the panel doesn't show "all stdout, then all stderr"). Older history beyond the limit isn't duplicated here, it's still reachable through the model's own `list_commands`/`read_terminal_output` tools:

```rust
pub struct SandboxOutputLine { stream: String, data: String }
pub struct SandboxCommandSummary { command_id: String, command: String, status: String, exit_code: Option<i32>, output: Vec<SandboxOutputLine> }
pub struct SandboxTerminalSummary { terminal_id: i64, pod_id: i64, status: String, commands: Vec<SandboxCommandSummary> }
pub struct SandboxPodSummary { pod_id: i64, status: String, terminals: Vec<SandboxTerminalSummary> }
pub struct SandboxSnapshot { pods: Vec<SandboxPodSummary> }
```

`subscribe_conversation_events` is a second, independent `ServerEvents` stream — unlike `send_message`'s, it isn't scoped to one request; a browser tab opens it once per viewed conversation and keeps it open for as long as that conversation is selected, forwarding whatever `events::subscribe(id)` yields:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConversationEvent {
    TaskUpdate { task_id: String, tool: String, status: String, stream: Option<String>, latest_output: Option<String> },
    MessagesAppended(Vec<Message>),
    SandboxPodUpdate { pod_id: i64, status: String, terminated: bool },
    SandboxTerminalUpdate { pod_id: i64, terminal_id: i64, status: String, terminated: bool },
    SandboxCommandUpdate { terminal_id: i64, command_id: String, command: Option<String>, status: String, exit_code: Option<i32>, stream: Option<String>, latest_output: Option<String> },
}
```

`TaskUpdate`/`Sandbox*` are all ephemeral UI telemetry (never persisted, regenerable at any time from `get_tasks`/`get_sandbox_state`); `MessagesAppended` is a live-delivery notification for rows `run_turn` already persisted — whether that `run_turn` call came from a live `send_message` or from a background task's own push. `SandboxPodUpdate`/`SandboxTerminalUpdate` fire on create/terminate (a `terminated: true` update means the frontend should *remove* that pod/terminal, not just relabel it — unlike a finished task, which the task panel keeps showing); `SandboxCommandUpdate` follows the exact same "started/one output line/finished" pattern `TaskUpdate` already uses, with `command` only populated on the "started" event. Since the underlying `broadcast` channel has no replay, the frontend does a one-shot `get_messages`/`get_tasks`/`get_sandbox_state` reconciliation pull on connect/reconnect to cover anything published before it subscribed — see `architecture.md`.

## Current endpoints

| Function | Method + path | Notes |
|---|---|---|
| `get_conversations` | `GET /api/conversations` | ordered by `updated_at DESC` |
| `create_conversation` | `POST /api/conversations` | default title |
| `get_messages` | `GET /api/conversations/{id}/messages` | ordered by `created_at ASC` |
| `send_message` | `POST /api/conversations/{id}/messages` | streams the assistant reply (and any tool-use turns), see above |
| `get_tasks` | `GET /api/conversations/{id}/tasks` | one-shot snapshot of `run_async` tasks for this conversation |
| `get_sandbox_state` | `GET /api/conversations/{id}/sandbox` | one-shot snapshot of every pod/terminal for this conversation, see above |
| `subscribe_conversation_events` | `GET /api/conversations/{id}/events` | always-open live stream, see above |
| `delete_conversation` | `DELETE /api/conversations/{id}` | hard delete; cascades to the conversation's messages (`ON DELETE CASCADE`); deleting a nonexistent id is not an error |

Not yet implemented (straightforward mechanical additions when needed): rename a conversation, concurrent-send guarding, wiring `delete_conversation` to cancel that conversation's still-running tasks.
