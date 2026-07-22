# Architecture

## Stack

Dioxus **fullstack** (SSR + hydration + typed server functions) on Axum, SQLite via sqlx, streaming to the real Anthropic Messages API via reqwest.

Unlike a hand-rolled Axum-API app, there is no separate REST layer and no hand-written browser fetch client: functions in `src/api/` decorated `#[get]`/`#[post]` are isomorphic — the same function is a real Axum route on the server build and a transparent HTTP call on the web/WASM build. See [api.md](api.md).

## Module map

| Module | Feature-gated? | Purpose |
|---|---|---|
| `src/main.rs` | — | Entry point. Server build: runs migrations, assembles the fullstack router, binds and serves. Web build: `dioxus::launch` boots the hydrated client. |
| `src/models.rs` | no | `Conversation`, `Message` — shared wire/row types, `sqlx::FromRow` derived only under `server`. |
| `src/db.rs` | `server` only | Global `OnceLock<SqlitePool>` (`init`/`get`) + CRUD functions. Mirrors the pattern used elsewhere in this style of app: handlers call `db::get()` directly rather than threading a pool through extractors. |
| `src/anthropic/types.rs` | no | Rust mirror of the Anthropic Messages API wire shapes (`ContentBlock`, `AnthropicMessage`, `CreateMessageRequest`, `CreateMessageResponse`). |
| `src/anthropic/stream.rs` | `server` only | `stream_anthropic_message`: calls the real Anthropic API with `stream: true`, parses its SSE event stream, calls back per text delta. |
| `src/api/chat.rs` | no (functions are isomorphic; only their bodies differ per build) | The server functions: `get_conversations`, `create_conversation`, `get_messages`, `send_message`. `send_message` returns a `ServerEvents<ChatEvent>` — see below. |
| `src/frontend/` | no | Dioxus components: `App` (router root), `pages::Chat` (`ConversationSidebar` + `ChatPanel`). |

## Two streams, not one

There are two distinct SSE-shaped things in this codebase and they don't talk to each other directly:

1. **Anthropic → smelt server** (`anthropic::stream`): the real Messages API's own SSE stream (`message_start`, `content_block_delta`, `message_stop`, ...), parsed down to plain text deltas.
2. **smelt server → browser** (`api::chat::send_message`'s `ServerEvents<ChatEvent>`): Dioxus fullstack's own SSE-based payload type for server functions. `send_message` relays each Anthropic text delta into a `ChatEvent::Delta`, then a final `ChatEvent::Done` (or `ChatEvent::Error`) once the reply is fully assembled and persisted.

The browser never sees Anthropic's wire format directly, and the Anthropic client never sees `ChatEvent`.

## Request flow: sending a message

1. `ChatPanel`'s send handler calls the `send_message(conversation_id, content)` server function. On the client this transparently becomes an HTTP POST; on the server it's the real function body.
2. The function stores the user's message, loads the conversation's history, and returns `ServerEvents::new(...)` — the closure inside runs as a background task on the server, calling `stream_anthropic_message` and forwarding each delta.
3. The client's copy of `send_message` receives that response as a live stream and iterates it with `.recv().await` in a loop, appending each `ChatEvent::Delta`'s text to a signal the UI renders live, then replacing it with the persisted message on `ChatEvent::Done`.

## Feature flags

Same two-target split as other Dioxus apps in this style: `server` (axum, sqlx, reqwest, tokio full) and `web` (wasm-targeted Dioxus). Every change must compile under both — see [development-process.md](development-process.md#definition-of-done).
