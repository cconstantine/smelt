# Architecture

## Stack

Dioxus **fullstack** (SSR + hydration + typed server functions) on Axum, Postgres via sqlx, streaming to the real Anthropic Messages API via reqwest.

Unlike a hand-rolled Axum-API app, there is no separate REST layer and no hand-written browser fetch client: functions in `src/api/` decorated `#[get]`/`#[post]`/`#[delete]` are isomorphic — the same function is a real Axum route on the server build and a transparent HTTP call on the web/WASM build. See [api.md](api.md).

The exceptions, all server-side: `build_router()` in `src/main.rs` adds one plain Axum route, `/oauth/mcp-callback/{id}` (the MCP OAuth redirect target, `mcp_oauth::callback_handler` — see [mcp.md](mcp.md)), and wraps the whole router in two layers, `request_guard::guard` (refuses cross-site writes and unknown `Host` names) and `tower_http`'s `TraceLayer` (each request's span: method and path only, see [Tracing](#tracing-opentelemetry)). Sandbox previews are served by `src/preview.rs`, a hyper reverse proxy on a listener of its own (`SMELT_PREVIEW_ADDR`), not by the Dioxus router.

## Module map

Every module, its feature gate and what it does, in two tables:

- [Modules: the app, chat, models and turns](architecture/modules-core.md)
- [Modules: the sandbox, browsing and the binaries](architecture/modules-sandbox.md)

## Live channels

Each SSE/broadcast-shaped stream answers a different question:

1. **Anthropic → smelt server** (`anthropic::stream`): the real Messages API's own SSE stream (`message_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`, ...), parsed down to text deltas and `tool_use` blocks. It never reaches the browser, and the browser never sees Anthropic's wire format.
2. **smelt server → browser, scoped to a conversation** (`api::chat::subscribe_conversation_events`'s `ServerEvents<ConversationEvent>`): "stream me whatever happens to this conversation, unprompted." A chat tab opens it once per viewed conversation and it forwards `events::subscribe(id)` for as long as that tab has the conversation selected, plus the app-wide `PodsChanged` and `TurnsChanged` relayed as `ConversationEvent`s. It carries every turn's messages and streaming reply text too, whoever started the turn: a tab's send, a finished command, another tab.
3. **smelt server → browser, app-wide** (`api::pods::subscribe_app_events`'s `ServerEvents<AppEvent>`): only the `/pods` page opens it.
4. **Browsing frames** (`api::browsing::subscribe_browser_frames`): the live screencast of the conversation's browsing session, open only while one is.

So a chat tab holds one always-open connection, plus the frames stream while the model is browsing.

**A tab older than the server** (SME-43). `build.rs` hashes `src/`, `assets/`, `Cargo.toml` and `Cargo.lock` into `SMELT_BUILD_ID` (`api::version::BUILD_ID`), the same in the server and the web bundle built from one tree. Each time stream 2 or 3 (re)connects, the tab calls `api::version::get_build_id`; a deploy restarts the server, which drops every stream, so every open tab asks the new one. A different id, or an event of a type the bundle doesn't know (`Unknown`, skipped without ending the stream), sets `frontend::STALE_BUNDLE`, and `App` shows "smelt was updated" with a Reload button. Nothing reloads by itself, and only a reload clears it. Any source change gives a new id, server-only ones included. `build.rs` also sets `SMELT_SANDBOX_IMAGE`, the sandbox image named after the agent's sources (`scripts/sandbox-image-ref`'s name), which a server with no `SANDBOX_IMAGE` runs (SME-121).

On a reconnect, the snapshot pulls bring back what the stream missed. The sandbox panel's snapshot also decides which pods and terminals exist: one that went away while the tab wasn't listening is dropped (`merge_sandbox_snapshot`).

## Request flow: sending a message

`send_message` is now a thin wrapper around `turn::run_turn`, a bounded loop (`MAX_TURNS`) rather than a single Anthropic call:

1. `ChatPanel`'s send handler shows the message at once (an optimistic copy with a negative id) and calls `send_message(conversation_id, content)`, an ordinary POST. On the server (`start_turn`) it checks the conversation exists, starts `run_turn` in a background task, and returns.
2. `run_turn` acquires a per-conversation async lock (`conversation_lock`, kept in the conversation's `turn::state` entry and held for its whole call), persists the new message, then loops: resolve the conversation's model the first time it's needed (see [api.md](api.md#which-model-a-turn-runs-on)), then call `stream_anthropic_message` on its provider with the full `tools` list (and, unless the model has it off, `thinking`) attached — retrying up to `TOOL_CALL_PARSE_RETRIES` times, first dropping `thinking` then just plain regenerating, if the upstream fails with the specific "error parsing tool call" shape a local model can produce; if `stop_reason == "tool_use"`, persist the assistant's `ToolUse` turn (and any `Thinking` block ahead of it, uninterpreted), run each tool via `anthropic::tools::execute`, persist the `ToolResult` turn, and continue; otherwise persist the final assistant turn and return every message persisted along the way. Each saved message is published as it's saved (`record_saved` → `ConversationEvent::MessagesAppended`), each model call starts with `ReplyReset` and streams `ReplyDelta`s, and a failed or stopped user turn ends with `TurnError`.
3. Every tab watching the conversation, the sender included, renders from those events: the streaming bubble from `ReplyReset`/`ReplyDelta`, and saved messages (the sender's own replacing its optimistic copy) from `MessagesAppended`.

**The lock exists because `run_turn` has more than one caller.** A notice that wakes the model (a terminal command finishing, a clone finishing) calls `run_turn` directly (through `turn::notify`), with no `send_message` request in flight at all. Two writers persisting a turn for the same conversation at once would break Anthropic's strict user/assistant alternation, so every `run_turn` call, whoever it's from, serializes through that conversation's lock. `run_turn` itself is a boxed, type-erased future (`Pin<Box<dyn Future<...> + Send>>`, not `async fn` sugar) — `run_turn` calls `anthropic::tools::execute`, and a tool's spawned work can wake the model, calling back into `run_turn`; that recursion defeats rustc's `Send`-auto-trait inference for plain `async fn`s without it.

A tool runs inside `run_turn`'s own tool-dispatch loop, which holds the conversation's lock, so a tool must never await a `run_turn` of its own conversation inline: it would wait on the lock its caller holds. Anything a tool sets off that wakes the model is spawned and runs once the turn releases the lock.

## Tracing (OpenTelemetry)

With `OTEL_EXPORTER_OTLP_ENDPOINT` set (the compose `smelt` service sets `http://tempo:4318`), smelt exports its `tracing` spans to Tempo, read in Grafana (see [setup/environment.md](setup/environment.md#tracing-opentelemetry)). Unset, nothing is built or sent, and only the console sees the spans (as prefixes on log lines, when `RUST_LOG` enables INFO). The span tree of a user's message:

```
POST /api/conversations/{id}/messages        (server; ends at its response)
└─ turn  conversation_id model provider_id turn.steps [turn.stopped]
   ├─ chat {model}   (client) gen_ai.* tokens, finish reason, smelt.retries, smelt.binding
   ├─ tool {name}    gen_ai.tool.name, gen_ai.tool.call.id, smelt.tool.mcp_server, smelt.tool.truncated
   │  ├─ start_or_get_pod → create_pod_attempt → wait_for_running ; agent connect ; agent request
   │  └─ mcp call (→ mcp connect)
   ├─ chat {model}
   └─ compaction → chat {model}
GET /api/mcp-servers/{id}/status → mcp connection_check → mcp connect
startup ; claim_sweep ; pricing_refresh ; pod_event ; pod_reconcile   (roots)
```

A wake (a finished command, an answered question) starts its `turn` as a root. A failure marks its span's status ERROR with the first 500 characters of the message (`telemetry::mark_error`); a stopped turn records `turn.stopped` and isn't an error. `warn!` and `error!` lines inside a span are exported as its events, and smelt's own `info!` lines too. Dependencies' INFO spans are not exported: dioxus-signals instruments every signal it makes.

**Every exported span runs through `telemetry::in_span`, not `Instrument::instrument`.** A span closes only when the last handle to it drops, and libraries hold handles past the work: hyper-util spawns each new pooled connection `in_current_span`, and rmcp's service loop keeps `Span::current()` for the connection's life. A span that opened a connection (and every parent) would stay open, unexported, until the connection closed. `in_span` ends the span's export when its future ends or is dropped; an HTTP request's span ends at its response. MCP connections are made `outside_spans`, so the cached service loop holds none of the turn's spans.

**Never recorded:** message text, tool input or output as such, MCP arguments or results, request headers, query strings (the OAuth callback's carries `code` and `state`), API keys, tokens, and `DATABASE_URL`'s credentials. An error's description is the message smelt has for it (the one the model gets, for a built-in tool), which can name part of the input, such as a path or a terminal id; every URL in it is cut to scheme, host and path (`telemetry::scrub_urls`). An MCP tool's own error output never goes on a span: its `mcp call` and `tool` spans say only that the tool reported an error. Log lines inside a span are exported with their fields as written, so a line that logs a URL scrubs it first (the request guard logs the path only, though its reason names the refused request's `Host`, `Origin` or `Sec-Fetch-Site` value: another site's names, not secrets).

## Feature flags

Same two-target split as other Dioxus apps in this style: `server` (axum, sqlx, reqwest, kube, rmcp, chromiumoxide, tokio full, ...) and `web` (wasm-targeted Dioxus, plus `gloo-timers` for timers such as the reconnect backoff — `tokio::time::sleep` has no driver on a browser tab, since there's no tokio runtime there at all). Every change must compile under both — see [process/done.md](process/done.md).

`browser-test` implies `server` and turns on the `#[ignore]`d real-browser tests (`src/browser_tests/`, `src/webfetch.rs`'s), so the default `cargo test --features server` stays fast. All three `[[bin]]`s (`sandbox_agent`, `sandbox_image_import`, `sandbox_instance_cleanup`) have `required-features = ["server"]`, so a featureless `cargo build` skips them.
