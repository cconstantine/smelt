# MCP client for externally-hosted servers

**Branch:** `mcp-servers`

## What

A generic MCP client: smelt is configured with one or more MCP servers it
doesn't run or manage — just a name, a URL, and a set of extra HTTP headers
to attach to every request, the same way `ANTHROPIC_BASE_URL`/
`ANTHROPIC_AUTH_TOKEN` configure smelt's connection to Claude. smelt
connects to each over HTTP (Streamable HTTP transport), and every tool
those servers expose shows up as an ordinary smelt tool — same
`tool_use`/`tool_result` round-trip, same transcript rendering, same
conversation lock — indistinguishable to the model from `read_file` or
`run_terminal_command`.

Auth is deliberately generic rather than a single "auth token" field: an
arbitrary set of extra headers (name/value pairs) attached to every request
to a given server. A bearer token is just `Authorization: Bearer <...>` set
as one of them — this covers that case (including GitHub's) without smelt
needing to know or guess the specific header shape a given MCP server
wants. Real OAuth (login flow, refresh) is a separate, later idea — see
`docs/projects/ideas/mcp-oauth.md`.

This is deliberately scoped to servers smelt merely *talks to*, not servers
smelt *runs*. An MCP server that needs direct access to a sandbox pod's own
filesystem (a git MCP server operating on the model's actual checkout, for
instance) is a different problem — spawning a process inside the pod,
wiring it a working directory, sandbox lifecycle — tracked separately as
`docs/projects/ideas/mcp-hosted-servers.md`, to be scoped once this client
exists to build on.

**GitHub's MCP server is this plan's concrete target** — the hosted server
at `https://api.githubcopilot.com/mcp/` (the same one referenced in
Anthropic's own Managed Agents MCP examples), authenticated by setting
`Authorization: Bearer <github-token>` as an extra header, or a self-hosted
`github-mcp-server` if the hosted endpoint turns out to need something a
static header genuinely can't express (see the spike in Open questions).
This project is not done when CI is green on mocked tests — it's done when
this repo's maintainer can configure smelt with a real GitHub MCP server and
watch a model, in an actual conversation, successfully call one of its
tools (e.g. list issues on a repo, read a file). See "Definition of done"
below.

## Which files

**New:**
- `migrations/<timestamp>_create_mcp_servers.sql` — the `mcp_servers` config
  table. See Data model below.
- `src/mcp.rs` — a thin wrapper around [`rmcp`](https://crates.io/crates/rmcp)
  (the official Rust MCP SDK — see "Building on `rmcp`" below), not a
  hand-rolled JSON-RPC/SSE client. Owns the connection registry keyed by DB
  row `id`, the `mcp__<server_name>__<tool_name>` naming translation, our
  `ClientHandler` impl for server-initiated notifications, and turning an
  `rmcp` tool-call result into a `ToolResult` string. There's no
  pod/`sandbox_agent` involvement anywhere here — this talks straight to the
  configured URL, the same trust level as talking to `ANTHROPIC_BASE_URL`.
- `src/api/mcp.rs` — server functions for managing the config table:
  `list_mcp_servers`, `create_mcp_server(name, url, extra_headers)`,
  `update_mcp_server(id, name, url, extra_headers)`, `delete_mcp_server(id)`.
  Same isomorphic-server-function shape as everything in `src/api/chat.rs`
  (see `docs/api.md`), just a separate file since this is a distinct
  resource, not conversation data.
- `src/frontend/pages/mcp_servers.rs` — the management page: a form to add a
  server (name, URL, a list of extra header name/value pairs) and a list of
  configured servers with edit/delete actions.

**Modified:**
- `Cargo.toml` — new dependency on `rmcp = "3.1.2"`,
  `default-features = false`, `features = ["client",
  "transport-streamable-http-client-reqwest"]` (confirmed via `cargo info
  rmcp` against the real published crate). A real new dependency, not a
  detail — see "Building on `rmcp`" below for why.
- `src/db.rs` — `McpServerConfig` struct plus
  `create_mcp_server_config`/`update_mcp_server_config`/
  `delete_mcp_server_config`/`list_mcp_server_configs`/
  `get_mcp_server_config`, following the plain CRUD shape `conversations`
  already uses (hard delete, no soft-delete `terminated_at` — this is
  config, not a live external resource like a pod).
- `src/anthropic/tools.rs`:
  - `tool_definitions()` becomes `async fn tool_definitions(pool: &PgPool) ->
    Vec<ToolDefinition>`. For each row in `db::list_mcp_server_configs`,
    lazily connects (or reuses a cached connection) and appends its tools,
    renamed `mcp__<server_name>__<tool_name>` to avoid colliding with
    smelt's own tool names or another server's tools.
  - `execute()` gains a fallback arm: any tool name starting with `mcp__` is
    parsed into `(server_name, tool_name)`, dispatched as an MCP `tools/call`
    via `src/mcp.rs`, and its result content translated into the
    `ToolResult` string (concatenate `content[].text` blocks; non-text
    content blocks get JSON-encoded inline rather than dropped).
- `src/api/chat.rs` — `run_turn_bounded`'s `tools:
  anthropic::tools::tool_definitions()` becomes `tools:
  anthropic::tools::tool_definitions(pool).await` — `pool` is already in
  scope there.
- `src/api/mod.rs` — `pub mod mcp;`
- `src/frontend/pages/mod.rs` — export the new page.
- `src/frontend/mod.rs` — new `Route::McpServersRoute` at `/mcp-servers`.
- `src/frontend/pages/chat.rs` — `ConversationSidebar` gets a nav link to
  the new page (it's the only place a persistent nav element exists today).
- `docs/api.md`, `docs/architecture.md`, `docs/frontend.md`,
  `docs/database.md`, `docs/projects/state.md` — updated once the feature
  lands, per the usual close-out.

**Not touched:** `src/sandbox.rs`, `src/bin/sandbox_agent.rs`.

## How

### Data model

```sql
CREATE TABLE mcp_servers (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,   -- the mcp__<name>__ prefix
    url            TEXT NOT NULL,
    extra_headers  JSONB NOT NULL DEFAULT '{}',  -- {"Authorization": "Bearer ..."} etc.
    created_at     TIMESTAMP NOT NULL DEFAULT now(),
    updated_at     TIMESTAMP NOT NULL DEFAULT now()
);
```

`extra_headers` is a flat `{header_name: header_value}` map, attached
verbatim to every `initialize`/`tools/list`/`tools/call` request `src/mcp.rs`
makes to that server — nothing header-specific is hardcoded (no
`Authorization`-only assumption), so whatever a given MCP server actually
wants (a bearer token, an API-key header, something else entirely) is just
another entry in the map.

Plain CRUD, no soft delete — this is configuration a person edits through
the UI, not a live external resource with its own lifecycle like a sandbox
pod, so there's nothing to "terminate" versus delete. Global, same as
before: every conversation sees every configured server's tools uniformly
(see Open questions on whether that's the right default going forward).

### Managing servers: UI + API

A new `/mcp-servers` page (linked from `ConversationSidebar`, the one
persistent nav surface today) lists configured servers and provides add /
edit / delete. Each add or edit round-trips through the new
`src/api/mcp.rs` server functions to `db::*_mcp_server_config`.

**Header values are write-only past the initial save.** `list_mcp_servers`
returns each configured header's *name* (so the user can see e.g. that
`Authorization` is set) but never its value — the same "never echoed"
treatment GitHub personal access tokens get in Anthropic's own MCP resource
docs. An edit replaces the whole `extra_headers` map wholesale only if the
form actually submits headers; leaving the header list as shown (names,
blanked values) keeps the stored values untouched rather than overwriting
them with nothing. Removing a header is an explicit per-row "remove" action
in the form, not just clearing its value field.

### Building on `rmcp` instead of hand-rolling the protocol

The original draft of this plan had `src/mcp.rs` hand-roll JSON-RPC framing,
`Mcp-Session-Id` tracking, and an SSE read loop mirrored from
`src/anthropic/stream.rs`. Checked instead: `rmcp`
(`https://github.com/modelcontextprotocol/rust-sdk`,
[crates.io](https://crates.io/crates/rmcp)) is the official Rust SDK for
MCP — actively maintained, ~4.7M downloads, and it already implements
Streamable HTTP (both response modes, so the JSON-vs-SSE branching the
original draft described is no longer smelt's problem), session handling,
and a `ClientHandler` trait specifically for server-initiated notifications
(see below). Hand-rolling all of that — and getting the edge cases right
against a real-world server whose exact behavior isn't confirmed yet (see
the GitHub spike in Open questions) — is a lot of protocol surface to own
for something the ecosystem already has a maintained, official
implementation of. Using it:

```rust
use rmcp::transport::streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientTransportConfig};

let config = StreamableHttpClientTransportConfig::with_uri(server.url.clone())
    .custom_headers(header_map_from(&server.extra_headers)); // confirmed API, see Open questions #7
let transport = StreamableHttpClientTransport::with_client(reqwest::Client::default(), config);
let client = ClientInfo::default().serve(transport).await?; // does the initialize handshake
let tools = client.list_all_tools().await?;
let result = client.call_tool(CallToolRequestParams { name: tool_name.into(), arguments: args }).await?;
```

(Constructor and config shape confirmed against the actual `rmcp` 3.1.2
source — see Open questions #7. `list_all_tools`/`call_tool`'s exact
signatures are confirmed to exist at those names too; the argument shapes
above are still approximate pending the real implementation pass.)

### Connection and dispatch flow

The in-memory registry idea from the original draft still holds — a
server's live connection doesn't belong in Postgres, it's re-derivable at
any time — but now it's a registry of `rmcp` client handles (`RunningService`
or equivalent) rather than a hand-tracked session id + tool list. Keyed by
the DB row's `id`, and has to react to **configuration that can change while
smelt is running**, not just be read once at startup:

- Lazily, on first use (either the first `tool_definitions()` call to see a
  given row, or the first `tools/call` against it): `rmcp` does the
  `initialize` handshake via `.serve(transport)`, smelt caches the resulting
  client handle and a `tools/list` snapshot in the registry, keyed by `id`.
- `tool_definitions(pool)` queries `db::list_mcp_server_configs` fresh on
  every call (cheap — a small table), reads the cached registry entry per
  row, triggering a lazy connect for any row not yet connected, and appends
  namespaced tool defs.
- `execute()`'s `mcp__` fallback looks up the registry entry by id and calls
  `call_tool` on the cached `rmcp` client handle. If the call fails (broken
  connection, session gone), drop the cached handle and reconnect once
  before retrying — the same reconnect-on-demand shape `sandbox.rs`'s
  `reconnect_if_needed` already uses for pod connections, just without a pod
  underneath it and without smelt owning the retry's protocol details.
- **`update_mcp_server`/`delete_mcp_server` evict that row's registry
  entry** (and drop/close the cached `rmcp` client handle). Editing a
  server's URL or headers while a stale cached connection still points at
  the old one would otherwise keep talking to whatever the server used to
  be — the edit API call itself is the trigger to drop the cache, not a TTL
  or a background poll.

### Server-initiated notifications

`rmcp`'s `ClientHandler` trait is the extension point for these — smelt
implements it once per connection (or a single shared impl parameterized by
the DB row `id`) rather than needing any protocol-level SSE/notification
parsing of its own. Two things happen with it in this pass:

- **`tools/list_changed` invalidates that row's cached tool list.** If a
  configured server reports its tools changed, the cached `tools/list`
  snapshot in the registry is refreshed on the next `tool_definitions()`
  call rather than staying stale until someone happens to edit the server's
  config. This is the concrete first payoff of handling notifications at
  all, not just plumbing for its own sake.
- **Everything else `ClientHandler` can report (progress, resource/prompt
  list changes, logging messages, ...) is accepted and logged via
  `tracing`, not acted on.** No product behavior is tied to them yet — that
  would be scope creep past what this plan needs — but the handler exists
  and won't drop or error on them, which is the actual bar "handle
  server-initiated notifications" sets for this pass: don't ignore the
  transport-level mechanism, don't necessarily build product features on
  top of every message type it can carry.

## Proving this works

**Automated tests** validate `src/mcp.rs`'s thin wrapper — registry
behavior, `mcp__` name translation, `execute()` dispatch, and the
`tools/list_changed` cache-invalidation path — against a small mock MCP
server, the same "mock the upstream" pattern `api::chat`'s existing tests
already use for the Anthropic API (see `docs/testing.md`). Since `rmcp` now
owns the actual wire protocol (JSON-RPC framing, both Streamable HTTP
response modes, notification dispatch), the risk these tests need to cover
shifted from "does smelt speak MCP correctly" to "does smelt's wrapper drive
`rmcp` correctly" — the mock server can most likely be built with `rmcp`'s
own server-side pieces (`ServerHandler`/`StreamableHttpService`) rather than
a hand-rolled `axum` handler, which also means the mock is exercising the
same client/server pairing a real deployment would, not a shape smelt
invented for its own convenience.

**Definition of done — not covered by CI, verified manually, same status as
the browser test tier's manual pass requirement (`docs/testing.md`):**
configure smelt with GitHub's real MCP server, start a conversation, and
have the model successfully call one of its tools end to end (e.g. list a
repo's open issues, read a file from a repo) — a real `tool_use` →
`tools/call` over the real network → real `tool_result` → the model using
that result in its reply. Mocked-server tests passing is necessary but not
sufficient; this manual check is the actual close-out gate for this
project.

## Open questions / tradeoffs

1. **Header values (bearer tokens included) sit in Postgres in plaintext.**
   Every secret elsewhere in this codebase is a server-side env var
   (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`) — nothing is stored in the
   database today. Storing `extra_headers` values as a DB column is a new
   posture for smelt, not just a new column. Given smelt is single-user with
   no login system and Postgres itself isn't exposed, this may be an
   acceptable trade for the UI/persistence the user asked for — but it's a
   real change worth confirming explicitly rather than landing implicitly,
   and is worth revisiting if smelt ever grows multi-user access or if
   `docs/projects/ideas/mcp-oauth.md` lands and needs to store a refresh
   token (a more sensitive, longer-lived secret than a single pasted PAT).
2. **Global vs. per-conversation application.** This plan assumes every
   configured server's tools are available in every conversation, uniformly
   — no per-conversation enable/disable. Now that there's a management UI
   anyway, a per-conversation toggle would be a natural extension of it, but
   still proposed as out of scope for this pass — confirm that's still the
   right call, or fold it in now while the UI is being built anyway.
3. **MCP session scope: one shared session per server, process-wide, vs. one
   per conversation.** Leaning toward shared/global for simplicity — most
   MCP servers use the session id for connection continuity, not
   conversation semantics — but flagging in case a real server we point this
   at expects session-per-conversation behavior.
4. **GitHub MCP server's actual auth mechanism — needs a spike, though
   `extra_headers` narrows the risk.** Generic headers mean the *mechanism*
   for injecting a credential is no longer in question — what's still
   unconfirmed from memory is whether `https://api.githubcopilot.com/mcp/`
   accepts a plain GitHub personal access token as a static
   `Authorization: Bearer <token>` header **at all**, or requires something
   tied to a Copilot subscription / a real OAuth flow that no static header
   value can satisfy (which would mean falling back to self-hosting
   `github-mcp-server` — which does take a plain PAT — as the target instead
   of the hosted endpoint, at least until `docs/projects/ideas/mcp-oauth.md`
   exists). Spike this early. Which Streamable HTTP response mode it uses
   (plain JSON or the SSE upgrade) is no longer a blocking question either
   way, now that `rmcp` handles both internally.
5. **Should the UI verify a server before saving it?** A "test connection"
   action on the add/edit form — running `initialize`/`tools/list` live and
   showing the discovered tools or a clear error before the row is
   persisted — would catch a bad URL/token immediately instead of only
   surfacing it the next time the model tries to use it. Proposed as a
   reasonable addition given the definition-of-done is "does this actually
   work against GitHub," but flagged as optional scope to confirm rather
   than assumed.
6. **No sandbox-panel-style live status for configured servers beyond the
   management page itself.** `ToolUse`/`ToolResult` blocks already render in
   the transcript for any tool, MCP-dispatched or not, so there's no silent
   gap in the conversation view. Whether the `/mcp-servers` page itself
   should show live status (last successful connection, cached tool count)
   versus just the raw config rows is a smaller version of the same
   question — lean toward showing it, since the page already has to fetch
   something to render a list, but not committing to the exact shape here.
7. ~~**`rmcp`'s exact API for the two things this plan actually needs from
   it is not yet confirmed against source**~~ **Resolved** — spiked against
   the actual `rmcp = "3.1.2"` source in
   `~/.cargo/registry/src/.../rmcp-3.1.2/src/`:
   - **Custom header injection** — confirmed.
     `StreamableHttpClientTransportConfig` (built via
     `StreamableHttpClientTransportConfig::with_uri(...)`, passed to
     `StreamableHttpClientTransport::with_client(client, config)`) has
     `.custom_headers(HashMap<HeaderName, HeaderValue>)` — exactly the
     generic shape `extra_headers` needs — plus a dedicated
     `.auth_header(bearer_token)` convenience method for the plain-bearer
     case (GitHub's, most likely). No fallback/custom `Transport` impl
     needed.
   - **`ClientHandler`'s notification method** — confirmed. It's
     `on_tool_list_changed(&self, context: NotificationContext<RoleClient>)
     -> impl Future<Output = ()> + ...` (`src/handler/client.rs`), default
     no-op (`impl ClientHandler for ()` compiles), so only this one method
     needs overriding for the cache-invalidation behavior described above.
   - **Bonus finding, not previously anticipated:**
     `StreamableHttpClientTransportConfig::reinit_on_expired_session`
     defaults to `true` — `rmcp` already retries once on an expired-session
     `404` (replay `initialize`, retry the in-flight request) before smelt's
     own reconnect-on-failure logic would even see the error. Simplifies
     "Connection and dispatch flow" above: smelt's own retry only needs to
     handle failures `rmcp`'s built-in recovery doesn't (a fully broken
     connection, not just an expired session).
   - Confirmed required feature flags via `cargo info rmcp`:
     `default-features = false`, `features = ["client",
     "transport-streamable-http-client-reqwest"]`.
