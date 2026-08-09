# Architecture

## Stack

Dioxus **fullstack** (SSR + hydration + typed server functions) on Axum, Postgres via sqlx, streaming to the real Anthropic Messages API via reqwest.

Unlike a hand-rolled Axum-API app, there is no separate REST layer and no hand-written browser fetch client: functions in `src/api/` decorated `#[get]`/`#[post]` are isomorphic — the same function is a real Axum route on the server build and a transparent HTTP call on the web/WASM build. See [api.md](api.md).

## Module map

| Module | Feature-gated? | Purpose |
|---|---|---|
| `src/main.rs` | — | Entry point. Server build: runs migrations, assembles the fullstack router, binds and serves. Web build: `dioxus::launch` boots the hydrated client. |
| `src/models.rs` | no | `Conversation`, `Message` — shared wire/row types, `sqlx::FromRow` derived only under `server`. `Message::blocks()` parses the stored JSON `content` column into `Vec<ContentBlock>` — see [models.md](models.md). |
| `src/db.rs` | `server` only | CRUD functions taking `pool: &PgPool` explicitly, plus a global `OnceLock<PgPool>` (`init`/`get`) for production wiring — `send_message` calls `db::get()` once and passes the pool through everything else, including `anthropic::tools::execute` and `run_async`'s spawned background tasks (see below). |
| `src/anthropic/types.rs` | no | Rust mirror of the Anthropic Messages API wire shapes: `ContentBlock` (`Text`/`ToolUse`/`ToolResult`), `AnthropicMessage`, `ToolDefinition`, `CreateMessageRequest` (now carries `tools`), `CreateMessageResponse`. |
| `src/anthropic/stream.rs` | `server` only | `stream_anthropic_message`: calls the real Anthropic API with `stream: true`, parses its SSE event stream (text deltas *and* `tool_use` blocks), and returns a `StreamedTurn { content: Vec<ContentBlock>, stop_reason: String }`. |
| `src/anthropic/tools.rs` | mixed (see below) | Tool implementations dispatched by name (`execute`) — `add`/`count` are throwaway protocol-proving stand-ins; `run_async` plus a `list_tasks`/`task_status`/`task_output`/`task_result`/`wait_task`/`cancel_task` suite (modeled on `ps`/`wait`/`kill`) is the reusable generic async-tool mechanism. `TaskSummary` (a plain data type used by the browser-facing `get_tasks`) is not feature-gated; the registry and `execute` itself are `server`-only, in a nested `mod server` re-exported under `#[cfg(feature = "server")]` — same shape `events.rs` uses, for the same reason (a type crossing the client/server boundary vs. the server-only logic that produces it). |
| `src/events.rs` | mixed (see below) | The per-conversation live event bus. `ConversationEvent` (not gated, crosses the boundary as `subscribe_conversation_events`'s payload) plus `server`-only `publish`/`subscribe` backed by a `LazyLock<Mutex<HashMap<i64, broadcast::Sender<ConversationEvent>>>>`. |
| `src/api/chat.rs` | no (functions are isomorphic; only their bodies differ per build) | The server functions: `get_conversations`, `create_conversation`, `get_messages`, `send_message`, `get_tasks`, `subscribe_conversation_events`, `delete_conversation`. `run_turn` (the turn loop both `send_message` and a background task's push call into) and the per-conversation lock live here too. |
| `src/frontend/` | no | Dioxus components: `App` (router root), `pages::Chat` (`ConversationSidebar` + `ChatPanel`). `ChatPanel` opens a live `subscribe_conversation_events` stream (web-only) and renders a background-tasks panel alongside the transcript. |
| `src/sandbox.rs` | `server` only | `Sandbox` (a live pod handle: `exec`, plus a `Drop` that queues cleanup) and `SandboxManager` (`create`/`delete`, owning the `kube::Client` and the cleanup queue's background drain task) — the coding-session sandbox lifecycle primitive against a k8s `smelt-park` namespace. Not yet wired to any tool; see [docs/projects/plans/k8s-sandbox.md](projects/plans/k8s-sandbox.md). |

## Three channels, not two

There are three distinct SSE/broadcast-shaped things in this codebase, each answering a different question:

1. **Anthropic → smelt server** (`anthropic::stream`): the real Messages API's own SSE stream (`message_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`, ...), parsed down to text deltas and `tool_use` blocks.
2. **smelt server → browser, scoped to one request** (`api::chat::send_message`'s `ServerEvents<ChatEvent>`): "stream me the reply to what I just sent." Relays each Anthropic text delta into a `ChatEvent::Delta`, then a `ChatEvent::Done` per message `run_turn` persisted (possibly more than one, if the turn looped through tool calls) or a `ChatEvent::Error`.
3. **smelt server → browser, scoped to a conversation** (`api::chat::subscribe_conversation_events`'s `ServerEvents<ConversationEvent>`): "stream me whatever happens to this conversation, unprompted." Not tied to sending anything — a browser tab opens it once per viewed conversation and it forwards `events::subscribe(id)` for as long as that tab has the conversation selected. This is what lets a background task's own turns (see below) and a second open tab both show up live, with no polling.

Channel 1 never reaches the browser directly, and the browser never sees Anthropic's wire format.

## Request flow: sending a message

`send_message` is now a thin wrapper around `api::chat::run_turn`, a bounded loop (`MAX_TURNS`) rather than a single Anthropic call:

1. `ChatPanel`'s send handler calls `send_message(conversation_id, content)`. On the client this transparently becomes an HTTP POST; on the server it's the real function body, which builds a synthetic `role: "user"` `AnthropicMessage` and returns `ServerEvents::new(...)` — the closure inside is spawned as a background task immediately (not lazily on first poll), calling `run_turn` with a live `on_delta` wired into the SSE channel.
2. `run_turn` acquires a per-conversation async lock (`CONVERSATION_LOCKS`, held for its whole call), persists the new message, then loops: call `stream_anthropic_message` with the full `tools` list attached; if `stop_reason == "tool_use"`, persist the assistant's `ToolUse` turn, run each tool via `anthropic::tools::execute`, persist the `ToolResult` turn, and continue; otherwise persist the final assistant turn and return every message persisted along the way. Each persisted batch triggers `events::publish(id, ConversationEvent::MessagesAppended(...))`.
3. The client's copy of `send_message` iterates the response with `.recv().await`, appending each `ChatEvent::Delta`'s text to a signal the UI renders live, then appending a real message on each `ChatEvent::Done`.

**The lock exists because `run_turn` has more than one caller now.** A background task started via `run_async` can call `run_turn` directly (with `on_delta = None`) to push a notification — a per-line update if the task was started with `stream_output: true`, and always a terminal "finished"/"failed"/"cancelled" notification — with no `send_message` request in flight at all. Two writers persisting a turn for the same conversation at once would break Anthropic's strict user/assistant alternation, so every `run_turn` call, whoever it's from, serializes through that conversation's lock. `run_turn` itself is a boxed, type-erased future (`Pin<Box<dyn Future<...> + Send>>`, not `async fn` sugar) — `run_turn` and `anthropic::tools::execute` call each other (a tool's background push calls back into `run_turn`, which calls `execute` again for the *next* turn's tools), and that mutual recursion defeats rustc's `Send`-auto-trait inference for plain `async fn`s without it.

One tool, `cancel_task`, runs synchronously inside `run_turn`'s own tool-dispatch loop (like any other tool call) but *also* needs to push a cancellation notification through `run_turn` — awaiting that inline would try to re-acquire the same non-reentrant lock the outer call is already holding and deadlock, so that specific push is `tokio::spawn`-detached instead, running once the outer call finishes and releases the lock.

## Feature flags

Same two-target split as other Dioxus apps in this style: `server` (axum, sqlx, reqwest, tokio full) and `web` (wasm-targeted Dioxus, plus `gloo-timers` for the reconnect backoff — `tokio::time::sleep` has no driver on a browser tab, since there's no tokio runtime there at all). Every change must compile under both — see [development-process.md](development-process.md#definition-of-done).
