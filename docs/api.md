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
            // chat_error_text: the error's own message, without ServerFnError's
            // "error running server function: … (details: None)" wrapper.
            Err(e) => { let _ = tx.send(ChatEvent::Error { message: chat_error_text(&e) }).await; }
        }
    }))
}
```

Requests carry `thinking: {"type": "adaptive"}` by default (`ANTHROPIC_THINKING=0` to turn it off — see [setup.md](setup.md)), so an assistant turn's `content` can start with a `ContentBlock::Thinking { thinking, signature }` block ahead of any `Text`/`ToolUse` blocks — `run_turn` persists and replays it exactly like any other block, uninterpreted; the frontend renders it as a collapsed-by-default `<details>` (see `frontend/pages/chat.rs`'s `render_block_element`).

If a request fails with Ollama's specific "error parsing tool call" 500 (its Anthropic-compat shim, at least for `gpt-oss` models, doesn't always turn a model's raw output into valid tool-call JSON — either because thinking's reasoning landed in the same text as the call, or the model just wrote invalid JSON on its own), `run_turn_bounded` retries: first with `thinking` dropped, then up to `TOOL_CALL_PARSE_RETRIES` further plain regenerations, since a local model's next sampling pass often doesn't repeat the same malformed output. Gives up and surfaces the error once that's exhausted. Any other failure propagates from `run_turn_bounded` immediately. See `is_ollama_thinking_tool_call_corruption`.

One layer down, `anthropic::stream::stream_anthropic_message` retries a briefly unavailable provider on its own: an HTTP 429, 503 or 529 gets two more attempts, 1s and then 3s later (`RETRY_DELAYS`), before anything has streamed. Any other status fails at once. A failure reads `model provider error {status}: {message}`, where the message is the provider's own (`provider_error_message` unwraps the JSON error body, including a nested one), not the raw response.

`run_turn_bounded` refuses a conversation that doesn't exist with a plain `conversation not found` (checked right after taking the conversation's lock), rather than letting the insert fail on a foreign key. `get_messages` returns the same error, which the chat page uses to show "This conversation doesn't exist" instead of a message box. The new message is saved *before* the credential check, so with no credentials configured the message is kept and only the reply fails.

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

Seven server functions exist purely to support live-updating panels (the background-tasks panel via `run_async`, the sandbox panel — see `docs/projects/completed/20260815-sandbox-visibility.md` — the context-usage indicator/detail view — see `docs/projects/completed/20260922-auto-compaction.md` — the todo panel via `todowrite`/`todoread` — see `docs/projects/completed/20260922-todo-list-tool.md` — and the browsing-session panel — see `docs/projects/completed/20260922-web-browsing.md`) and turns pushed from outside a request:

```rust
#[get("/api/conversations/{id}/tasks")]
pub async fn get_tasks(id: i64) -> ServerFnResult<Vec<anthropic::tools::TaskSummary>>;

#[get("/api/conversations/{id}/todos")]
pub async fn get_todos(id: i64) -> ServerFnResult<Vec<anthropic::tools::TodoItem>>;

#[get("/api/conversations/{id}/sandbox")]
pub async fn get_sandbox_state(id: i64) -> ServerFnResult<SandboxSnapshot>;

#[get("/api/conversations/{id}/context-usage")]
pub async fn get_context_usage(id: i64) -> ServerFnResult<ContextUsageSnapshot>;

#[get("/api/conversations/{id}/context-detail")]
pub async fn get_context_detail(id: i64) -> ServerFnResult<ContextDetailSnapshot>;

#[get("/api/conversations/{id}/browsing")]
pub async fn get_browsing_state(id: i64) -> ServerFnResult<browsing::BrowsingState>;

#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(id: i64) -> ServerFnResult<ServerEvents<ConversationEvent>>;
```

`get_tasks` is a thin, one-shot wrapper around `anthropic::tools::snapshot_tasks` — every task started via `run_async` in that conversation, with its current status. `get_todos` is the same one-shot shape for the todo panel: the conversation's current todo list, as last set by `todowrite` (`db::get_conversation_todos` — empty if never called). `get_sandbox_state` is the same shape for the sandbox panel: every pod and terminal currently live in the conversation, each terminal hydrated with its `HISTORY_LIMIT` most recent commands (oldest first), each with the last 200 lines per stream, merged back into one true chronological `output` (stdout and stderr are fetched/capped independently so one stream can't crowd the other out of the window, then re-sorted by `seq` — see `fetch_command_summary` — so the panel doesn't show "all stdout, then all stderr"). Older history beyond the limit isn't duplicated here, it's still reachable through the model's own `list_commands`/`read_terminal_output` tools:

```rust
pub struct SandboxOutputLine { stream: String, data: String }
pub struct SandboxCommandSummary { command_id: String, command: String, status: String, exit_code: Option<i32>, output: Vec<SandboxOutputLine> }
pub struct SandboxTerminalSummary { terminal_id: i64, pod_id: i64, status: String, commands: Vec<SandboxCommandSummary> }
pub struct SandboxPodSummary { pod_id: i64, status: String, terminals: Vec<SandboxTerminalSummary> }
pub struct SandboxSnapshot { pods: Vec<SandboxPodSummary> }
```

`get_context_usage` is the always-visible indicator's one-shot pull — the last real `usage` numbers persisted for this conversation (`None` if no turn has completed yet) plus the configured model's context window. `get_context_detail` is the click-through view: the same usage numbers, plus the system prompt (currently always `None`), every available tool's full definition, and the current message count — reconstructed from current state each call, not a stored snapshot of some specific past request:

```rust
pub struct ContextUsageSnapshot { usage: Option<anthropic::TokenUsage>, context_window: u32 }
pub struct ContextDetailSnapshot { system: Option<String>, tools: Vec<anthropic::ToolDefinition>, message_count: usize, usage: Option<anthropic::TokenUsage>, context_window: u32 }
```

`get_browsing_state` is the browsing panel's own one-shot check — is a session open right now, and on what URL (`browsing::current_url`) — so the panel can show an idle state instead of trying to subscribe to a frame stream that doesn't exist yet, and fill its address bar:

```rust
pub struct BrowsingState { session_open: bool, url: Option<String> }
```

`subscribe_conversation_events` is a second, independent `ServerEvents` stream — unlike `send_message`'s, it isn't scoped to one request; a browser tab opens it once per viewed conversation and keeps it open for as long as that conversation is selected, forwarding whatever `events::subscribe(id)` yields:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConversationEvent {
    TaskUpdate { task_id: String, tool: String, status: String, stream: Option<String>, latest_output: Option<String> },
    MessagesAppended { messages: Vec<Message> },
    SandboxPodUpdate { pod_id: i64, status: String, terminated: bool },
    SandboxTerminalUpdate { pod_id: i64, terminal_id: i64, status: String, terminated: bool },
    SandboxCommandUpdate { terminal_id: i64, command_id: String, command: Option<String>, status: String, exit_code: Option<i32>, stream: Option<String>, latest_output: Option<String> },
    NotificationDeliveryFailed { detail: String },
    ContextUsageUpdate { usage: anthropic::TokenUsage, context_window: u32 },
    TodoListUpdate { items: Vec<anthropic::tools::TodoItem> },
    BrowsingSessionUpdate { open: bool },
    BrowsingUrlUpdate { url: String },
}
```

`MessagesAppended` is a struct variant, not `MessagesAppended(Vec<Message>)`: the enum is internally tagged, and serde can't write a tuple variant holding a list that way. It used to be one, and every send failed silently (`wire_tests::test_every_event_round_trips_through_json` now serializes one of each variant). The stream is built with `ServerEvents::from_stream` over the broadcast receiver, for the same reason as the frame stream below: a closed connection drops the subscription. `NotificationDeliveryFailed` is published when a turn fired with no request in flight fails: `wake_conversation` (a terminal command finished) and a `run_async` task's output or completion notification.

`TaskUpdate`/`Sandbox*`/`ContextUsageUpdate`/`TodoListUpdate`/`BrowsingSessionUpdate`/`BrowsingUrlUpdate` are all ephemeral UI telemetry (never persisted as such, regenerable at any time from `get_tasks`/`get_sandbox_state`/`get_context_usage`/`get_todos`/`get_browsing_state` — `ContextUsageUpdate`'s own numbers *are* separately persisted, in `conversation_context_usage`, precisely so `get_context_usage` can regenerate it, and `TodoListUpdate`'s are likewise persisted in `conversation_todos`); `MessagesAppended` is a live-delivery notification for rows `run_turn` already persisted — whether that `run_turn` call came from a live `send_message` or from a background task's own push. `SandboxPodUpdate`/`SandboxTerminalUpdate` fire on create/terminate (a `terminated: true` update means the frontend should *remove* that pod/terminal, not just relabel it — unlike a finished task, which the task panel keeps showing); `SandboxCommandUpdate` follows the exact same "started/one output line/finished" pattern `TaskUpdate` already uses, with `command` only populated on the "started" event. `ContextUsageUpdate` publishes once per completed real turn, right after `run_turn_bounded` persists that turn's usage. `TodoListUpdate` publishes on every `todowrite` call and always carries the *complete* current list (never a partial diff — `todowrite` itself is a whole-list replace, no per-item ids), so the frontend just overwrites its signal wholesale rather than merging like `TaskUpdate` requires. `BrowsingSessionUpdate` publishes from `open_browser_session`/`close_browser_session`, so the panel shows/hides reactively rather than only checking on conversation (re)select. Since the underlying `broadcast` channel has no replay, the frontend does a one-shot `get_messages`/`get_tasks`/`get_sandbox_state`/`get_context_usage`/`get_todos`/`get_browsing_state` reconciliation pull on connect/reconnect to cover anything published before it subscribed — see `architecture.md`.

### The browsing panel's own live channel

Unlike every other panel above, the browsing panel's live frames are **not** part of `ConversationEvent` — they're frequent, ephemeral, UI-only data (a base64 JPEG per screen update) that shouldn't crowd out task/message updates in that shared bounded broadcast channel, so they get a dedicated per-conversation channel instead, in `src/browsing.rs` itself:

```rust
#[get("/api/conversations/{id}/browsing/frames")]
pub async fn subscribe_browser_frames(id: i64) -> ServerFnResult<ServerEvents<browsing::BrowserFrame>>;

#[post("/api/conversations/{id}/browsing/input")]
pub async fn send_browser_input(id: i64, event: browsing::BrowserInputEvent) -> ServerFnResult<()>;

#[post("/api/conversations/{id}/browsing/navigate")]
pub async fn navigate_browser(id: i64, address: String) -> ServerFnResult<()>;
```

`navigate_browser` is the address bar's endpoint. A bare host gets `https://` (`browsing::normalize_address`); then it runs the same `browsing::navigate` the model's `browser_navigate` uses, so the scheme check and SSRF guard apply identically. The address bar learns the resulting URL the same way it learns about every other navigation: `BrowsingUrlUpdate`, published whenever the session page's main-frame URL changes (`frameNavigated`, plus `navigatedWithinDocument` for `pushState`/fragment changes). A failed load reports the address that failed, not Chrome's `chrome-error://` page.

`subscribe_browser_frames` starts the real CDP screencast (`Page.startScreencast`) the moment the *first* viewer subscribes and stops it (`Page.stopScreencast`) when the *last* one disconnects — reference-counted, via a `Drop`-based guard inside `browsing::FrameSubscription`. The count only flips a flag; one long-lived task per session (`browsing::run_screencast`) issues every start/stop command itself, so a stop meant for a departed viewer can never land after a new viewer's start. Unlike `send_message`/`subscribe_conversation_events`, it's built with `ServerEvents::from_stream`, not `ServerEvents::new`, and that choice matters: `new` runs its closure as a detached task feeding an unbounded queue, so a closed connection is never noticed (the viewer would never be released) and a slow reader would build an unbounded backlog of frames. With `from_stream` the response body *pulls* each frame only when the connection can take it: a slow viewer just gets the newest frame on its next pull, and a closed connection drops the stream and the viewer with it. Frames travel over a `watch` channel holding only the latest frame, not a broadcast channel: Chrome only sends a frame when something on screen changes, so a viewer joining a running screencast on a still page (a second tab, a reload) would otherwise wait forever. With `watch`, a new viewer gets the current frame at once. `send_browser_input` forwards one mouse/keyboard event (in the frame's own fixed pixel space — the session's viewport is pinned to match the screencast bounds exactly, so no scale-factor lookup is needed) to the real page via `Input.dispatchMouseEvent`/`dispatchKeyEvent`/`insertText`. The panel sends these one request at a time, in order (see [frontend.md](frontend.md#the-live-browsing-panel)), since separately issued requests can overtake each other. There's no locking between this and the model's own tool-driven `browser_click`/`browser_fill`/... calls — both act on the same real page, and CDP just serializes whichever commands arrive.

## Current endpoints

| Function | Method + path | Notes |
|---|---|---|
| `get_conversations` | `GET /api/conversations` | ordered by `updated_at DESC` |
| `create_conversation` | `POST /api/conversations` | default title |
| `get_messages` | `GET /api/conversations/{id}/messages` | ordered by `created_at ASC` |
| `send_message` | `POST /api/conversations/{id}/messages` | streams the assistant reply (and any tool-use turns), see above |
| `get_tasks` | `GET /api/conversations/{id}/tasks` | one-shot snapshot of `run_async` tasks for this conversation |
| `get_todos` | `GET /api/conversations/{id}/todos` | one-shot snapshot of the current todo list, see above |
| `get_sandbox_state` | `GET /api/conversations/{id}/sandbox` | one-shot snapshot of every pod/terminal for this conversation, see above |
| `get_context_usage` | `GET /api/conversations/{id}/context-usage` | one-shot snapshot for the always-visible context-usage indicator, see above |
| `get_context_detail` | `GET /api/conversations/{id}/context-detail` | one-shot snapshot for the context-usage detail view, see above |
| `get_browsing_state` | `GET /api/conversations/{id}/browsing` | one-shot check for whether a browsing session is open, and its URL, see above |
| `subscribe_browser_frames` | `GET /api/conversations/{id}/browsing/frames` | live screencast frame stream for the browsing panel, see above |
| `send_browser_input` | `POST /api/conversations/{id}/browsing/input` | forwards one live-panel mouse/keyboard event, see above |
| `navigate_browser` | `POST /api/conversations/{id}/browsing/navigate` | the live panel's address bar, see above |
| `subscribe_conversation_events` | `GET /api/conversations/{id}/events` | always-open live stream, see above |
| `delete_conversation` | `DELETE /api/conversations/{id}` | hard delete; cascades to the conversation's messages (`ON DELETE CASCADE`); also tears down its sandboxes and browsing session, stops its `run_async` tasks, and drops its event channel and turn lock; deleting a nonexistent id is not an error |

Not yet implemented (straightforward mechanical additions when needed): rename a conversation, concurrent-send guarding.
