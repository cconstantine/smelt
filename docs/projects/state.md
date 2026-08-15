# Current State

## What smelt is

A single-user, 100%-Rust AI chat agent: a Dioxus fullstack (SSR + hydration) web app talking to Claude via the Anthropic Messages API, with Postgres-persisted conversation history, token-by-token streamed replies, and tool-use — the agent can call tools mid-conversation, including ones that run in the background. No login system — the Anthropic API key is a server-side env var (`ANTHROPIC_API_KEY`).

## Features

- Create conversations, switch between them (sidebar list, ordered by most-recently-active)
- Each conversation has its own URL (`/conversation/{id}`) — a refresh, a bookmark, or a direct link lands back on the same conversation, server-rendered; `/` is the empty "select or start a conversation" state. See [frontend.md](../frontend.md).
- Delete a conversation from the sidebar (inline confirm, hard delete, cascades to its messages)
- Send a message, see it appear immediately, watch the assistant's reply stream in live
- The agent can call tools mid-conversation (`add`, `count` are still stand-ins proving the protocol; the sandbox terminal tools below are the first real ones) and the conversation loop round-trips `tool_use`/`tool_result` blocks automatically, up to a bounded number of turns per send
- Extended thinking (`ContentBlock::Thinking`) is on by default — the model's reasoning renders as a collapsed-by-default disclosure ahead of its reply, never forced open. `ANTHROPIC_THINKING=0` turns it off. Since a local Ollama model's Anthropic-compat shim can occasionally fail to keep its reasoning separate from a tool call's own JSON (a real failure mode, not hypothetical), `run_turn` retries — dropping `thinking`, then a few plain regenerations — before giving up; see [api.md](../api.md) and [setup.md](../setup.md).
- The agent can also run a tool in the background via `run_async` (`fork`+`exec` for a tool call) and manage it afterward with a small `ps`/`wait`/`kill`-style suite (`list_tasks`, `task_status`, `task_output`, `task_result`, `wait_task`, `cancel_task`) — it gets notified in the conversation automatically when a background task finishes, with no further tool call required
- The agent can create any number of disposable Kubernetes-pod sandboxes per conversation, each with any number of persistent terminals in it (`create_pod`/`terminate_pod`/`list_pods`, `create_terminal`/`terminate_terminal`/`list_terminals`), and run real shell commands in them (`run_terminal_command`, `send_signal`, `terminal_command_status`, `read_terminal_output`, `list_commands`) — a real, stateful shell (cwd, exported variables persist across separate commands and across turns), output pulled on demand rather than force-fed into every request, with a completion notification pushed automatically when a background command finishes. See [projects/completed/20260812-sandbox-terminal.md](completed/20260812-sandbox-terminal.md).
- A live sandbox panel shows every pod and terminal the model currently has, each terminal's command history with stdout/stderr interleaved in the true order they were written (not grouped by stream), streaming in as it happens — no reload needed. More than one pod gets a tab bar instead of stacking; the whole panel goes side-by-side with the chat transcript above a width threshold, and stacks below it (sandbox on top, chat on the bottom) under it. See [projects/completed/20260815-sandbox-visibility.md](completed/20260815-sandbox-visibility.md).
- A live, always-open per-conversation event stream pushes new messages, background-task status, and sandbox pod/terminal/command status to the browser the instant they happen — including turns a background task pushed with no `send_message` request in flight — so a browser tab stays current without polling
- Conversation auto-titled from its first user message
- History persists across restarts (Postgres, not in-memory) — message content is stored as JSON `ContentBlock`s, not plain text, so tool calls, results, and thinking blocks round-trip through storage exactly like Anthropic's own protocol shape
- Missing/invalid API key surfaces as a visible error in the chat UI rather than hanging or crashing silently
- `ANTHROPIC_BASE_URL` can point at a local Ollama server instead of the real Anthropic API (v0.14.0+ serves an Anthropic-compatible `/v1/messages`) — see [setup.md](../setup.md).

## Architecture (short version — see [architecture.md](../architecture.md) for the full picture)

- Dioxus fullstack: server functions in `src/api/chat.rs` are the entire API surface — no hand-rolled Axum handlers, no hand-written browser fetch client.
- `send_message` streams via `ServerEvents<ChatEvent>`; a second, independent stream, `subscribe_conversation_events`, pushes `ConversationEvent`s (new messages, task status) to any browser tab that's watching a conversation, whether or not it's the one that sent anything.
- `send_message` is a thin wrapper around `run_turn`, a bounded (`MAX_TURNS`) loop that round-trips `tool_use`/`tool_result` turns with the real Anthropic API and persists each one. A per-conversation async lock serializes every writer — a live request and a background task's push notification can both want to persist a turn at the same time.
- `src/anthropic/` talks to the real Anthropic API separately, reducing its SSE stream down to text deltas and `tool_use` blocks before they ever reach `ChatEvent`.
- `src/anthropic/tools.rs` dispatches tool calls by name and owns the in-memory background-task registry `run_async` spawns into.
- `src/events.rs` is the per-conversation broadcast bus (`tokio::sync::broadcast`) that `subscribe_conversation_events` forwards and both `tools.rs` and `chat.rs` publish to.
- `src/db.rs`: CRUD functions taking a `&PgPool` parameter, plus a global `OnceLock<PgPool>` for production wiring, no request-scoped extractor plumbing.
- `src/sandbox.rs`: `Sandbox`/`SandboxManager` — create/exec/delete a disposable Kubernetes Pod, in a `smelt-park` namespace (a hermetic `docker-compose.yml`-provided `k3s` cluster for tests, the real `homelab` cluster for production). See [projects/completed/20260809-k8s-sandbox.md](completed/20260809-k8s-sandbox.md). Built on top of that: pod/terminal/command as three separately-guarded lifecycles, N pods per conversation each with N terminals, a small `axum` agent binary (`src/bin/sandbox_agent.rs`) injected into each pod and multiplexing every terminal that pod hosts over one WebSocket connection. See [projects/completed/20260812-sandbox-terminal.md](completed/20260812-sandbox-terminal.md). The live sandbox panel (pods/terminals/commands, streaming output) is layered on top of this — see [projects/completed/20260815-sandbox-visibility.md](completed/20260815-sandbox-visibility.md).
- `src/frontend/mod.rs`: the `Route` enum (`Home` at `/`, `ConversationRoute` at `/conversation/{id}`) — `Chat` derives which conversation is selected from the router itself (`use_memo` over `router.current::<Route>()`), not a plain owned signal, so it stays correct across client-side navigation as well as a fresh load.

## Explicitly out of scope (v1)

- **`add`/`count`** exist only to prove the tool-use protocol and the async-task mechanism round-trip correctly through this codebase; neither does anything useful. The sandbox terminal tools are the first real ones — see `projects/ideas/coding-session.md` for what's still open on top of them (file read/write, a coding-oriented system prompt, `git clone`/credentials).
- Multi-user accounts or login.
- Renaming conversations. (Deleting is supported — see [api.md](../api.md).)
- Durable/crash-recoverable background tasks — the task registry `run_async` uses is in-memory only; a server restart silently loses any task in flight (see the tool-use-round-trip plan's retrospective for the full list of gaps this leaves: no output pagination, no concurrent-task cap, no eviction policy, `cancel_task`'s race with natural completion).
- Coalescing for `run_async`'s `stream_output` option — every streamed line currently costs a full model round trip, workable only because `count`'s target/interval are both clamped to a handful. Not safe to point at a real, higher-volume tool yet.

## Goals for what comes next

The sandbox terminal tools give the model a real, persistent shell, and the sandbox panel makes that visible live in the browser, but `projects/ideas/coding-session.md`'s broader vision still has open pieces: file read/write tools (so the model can edit code, not just run commands that happen to touch files), a coding-oriented `system` prompt so every conversation is a coding session by default, and `git clone`/credential wiring so a session can actually check out a repo. Any of these is a reasonable next project; the terminal tools' pull-based design and pod/terminal/command lifecycle are meant to carry over as-is under whichever comes first.
