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

Two new server functions exist purely to support the async-tool mechanism (`run_async` and its task-management suite — see `docs/projects/state.md`) and turns pushed from outside a request:

```rust
#[get("/api/conversations/{id}/tasks")]
pub async fn get_tasks(id: i64) -> ServerFnResult<Vec<anthropic::tools::TaskSummary>>;

#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(id: i64) -> ServerFnResult<ServerEvents<ConversationEvent>>;
```

`get_tasks` is a thin, one-shot wrapper around `anthropic::tools::snapshot_tasks` — every task started via `run_async` in that conversation, with its current status. `subscribe_conversation_events` is a second, independent `ServerEvents` stream — unlike `send_message`'s, it isn't scoped to one request; a browser tab opens it once per viewed conversation and keeps it open for as long as that conversation is selected, forwarding whatever `events::subscribe(id)` yields:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConversationEvent {
    TaskUpdate { task_id: String, tool: String, status: String, latest_output: Option<String> },
    MessagesAppended(Vec<Message>),
}
```

`TaskUpdate` is ephemeral UI telemetry for the background-tasks panel (never persisted, regenerable at any time from `get_tasks`); `MessagesAppended` is a live-delivery notification for rows `run_turn` already persisted — whether that `run_turn` call came from a live `send_message` or from a background task's own push. Since the underlying `broadcast` channel has no replay, the frontend does a one-shot `get_messages`/`get_tasks` reconciliation pull on connect/reconnect to cover anything published before it subscribed — see `architecture.md`.

## Current endpoints

| Function | Method + path | Notes |
|---|---|---|
| `get_conversations` | `GET /api/conversations` | ordered by `updated_at DESC` |
| `create_conversation` | `POST /api/conversations` | default title |
| `get_messages` | `GET /api/conversations/{id}/messages` | ordered by `created_at ASC` |
| `send_message` | `POST /api/conversations/{id}/messages` | streams the assistant reply (and any tool-use turns), see above |
| `get_tasks` | `GET /api/conversations/{id}/tasks` | one-shot snapshot of `run_async` tasks for this conversation |
| `subscribe_conversation_events` | `GET /api/conversations/{id}/events` | always-open live stream, see above |
| `delete_conversation` | `DELETE /api/conversations/{id}` | hard delete; cascades to the conversation's messages (`ON DELETE CASCADE`); deleting a nonexistent id is not an error |

Not yet implemented (straightforward mechanical additions when needed): rename a conversation, concurrent-send guarding, wiring `delete_conversation` to cancel that conversation's still-running tasks.
