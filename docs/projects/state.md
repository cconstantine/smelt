# Current State

## What smelt is

A single-user, 100%-Rust AI chat agent: a Dioxus fullstack (SSR + hydration) web app talking to Claude via the Anthropic Messages API, with SQLite-persisted conversation history and token-by-token streamed replies. No login system — the Anthropic API key is a server-side env var (`ANTHROPIC_API_KEY`).

## Features

- Create conversations, switch between them (sidebar list, ordered by most-recently-active)
- Delete a conversation from the sidebar (inline confirm, hard delete, cascades to its messages)
- Send a message, see it appear immediately, watch the assistant's reply stream in live
- Conversation auto-titled from its first user message
- History persists across restarts (SQLite, not in-memory)
- Missing/invalid API key surfaces as a visible error in the chat UI rather than hanging or crashing silently

## Architecture (short version — see [architecture.md](../architecture.md) for the full picture)

- Dioxus fullstack: server functions in `src/api/chat.rs` are the entire API surface — no hand-rolled Axum handlers, no hand-written browser fetch client.
- `send_message` streams via `ServerEvents<ChatEvent>`, Dioxus fullstack's native SSE payload type for server functions.
- `src/anthropic/` talks to the real Anthropic API separately, reducing its SSE stream down to plain text deltas before they ever reach `ChatEvent`.
- `src/db.rs`: global `OnceLock<SqlitePool>`, plain CRUD functions, no request-scoped extractor plumbing.

## Explicitly out of scope (v1)

- Tool-use / function calling — the agent can only talk, not act. This is the planned next step.
- Multi-user accounts or login.
- Renaming conversations. (Deleting is supported — see [api.md](../api.md).)
- An automated browser test tier (manual verification only so far — see [testing.md](../testing.md)).

## Goals for what comes next

Tool-use is the natural next project: give the agent one or two real tools (candidates: web fetch/search, a local filesystem or shell tool) and extend the conversation loop to handle `tool_use`/`tool_result` blocks — most of the wire types in `anthropic::types` were deliberately kept close to the full Anthropic shape (a `ContentBlock` enum with room to add variants) specifically so this is additive, not a rewrite.
