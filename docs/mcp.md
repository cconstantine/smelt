# MCP servers

smelt is an MCP client (SME-15): the model can use tools from externally hosted MCP servers, configured on the `/mcp-servers` page. [`rmcp`](https://crates.io/crates/rmcp), the official Rust MCP SDK, owns the protocol (JSON-RPC, Streamable HTTP, sessions, notifications) and the OAuth client. smelt's side is:

- `src/mcp.rs`: the connection registry, tool naming, calling a tool and turning its result into a `ToolResult` string.
- `src/mcp_oauth.rs`: OAuth wiring around `rmcp::transport::auth` (SME-16).
- `src/api/mcp.rs`: the `/mcp-servers` pages' server functions (listed in [api.md](api.md#current-endpoints)).

Only remote servers over Streamable HTTP are supported; nothing runs an MCP server locally.

## Configuration: the `mcp_servers` table

One row per server. Plain CRUD, no soft delete: it's configuration, not a live resource.

| Column | |
|---|---|
| `name` | unique; the `<server>` in tool names |
| `url` | the server's Streamable HTTP endpoint |
| `extra_headers` | JSONB map, sent as-is on every request (e.g. `Authorization`, `x-api-key`) |
| `auth_mode` | `static_headers` (default) or `oauth`, `CHECK`ed |
| `oauth_credentials` | JSONB: `rmcp`'s `StoredCredentials` as-is (client id, tokens, scopes, issuer) |
| `oauth_client_id`, `oauth_client_secret` | a client registered by hand with the provider; set only at creation |

**Header values and the client secret are write-only.** The browser gets `McpServerSummary`: header *names* only, and `has_oauth_client_secret` instead of the secret. So `update_mcp_server` doesn't take a full header map: it takes `upsert_headers` and `remove_headers`, merged into the stored map (`db::update_mcp_server_config`), and every other header keeps the value the browser never saw. The client id isn't secret (it's in the authorization URL) and is shown back plain. The page lets you pick `auth_mode` only when creating a server.

An update or delete calls `mcp::evict(id)`, so a connection to the old URL or with old headers is never reused. An update that changes the URL also clears `oauth_credentials`: a grant belongs to the resource it was issued for.

### The built-in Exa server

At startup `mcp::ensure_default_servers` inserts each of `DEFAULT_MCP_SERVERS` that isn't there yet, by name (`db::ensure_mcp_server`, `ON CONFLICT (name) DO NOTHING`). Today that's `exa` at `EXA_MCP_URL`, keyless web search (SME-24). A failure is logged, not fatal. What this means for the user (keyless limits, edits kept, deleting doesn't stick) is in [setup.md](setup.md#built-in-mcp-servers).

## Connections

`REGISTRY` holds one `Connection` per server id: the running `rmcp` service, its cached tool list, and a `stale` flag. It's in memory and filled lazily: nothing connects at startup, only when a turn lists tools, a tool is called, or the status check runs. A restart just means connecting again.

`ensure_connected` is the one path in:
- **Already connected and not stale:** nothing to do.
- **Stale** (the server sent `tools/list_changed`, which `SmeltClientHandler` turns into setting the flag): re-list the tools on the same connection. If listing fails, the connection is taken as broken, dropped and reconnected. Every other server notification uses `rmcp`'s no-op defaults.
- **Not connected:** connect under a per-server lock (`CONNECTING`), so two callers don't both connect and a slow server holds up only callers that want it, not the whole registry (SME-40). `connect` builds the transport with the headers (OAuth adds a bearer token, below), runs the `initialize` handshake and lists all tools. `retry_once` tries once more on failure, since a first connect is the one most likely to hit a one-off problem.

Limits (SME-51); the three timeouts are 1s under `cfg(test)`:

| Constant | Value | Bounds |
|---|---|---|
| `CONNECT_TIMEOUT` | 15s | connecting plus listing tools, per attempt |
| `TOOL_LIST_WAIT` | 10s | how long a turn waits for a server when listing tools; a connect that takes longer carries on in the background and its tools show up on a later call |
| `CALL_TIMEOUT` | 120s | one tool call (which holds the turn and the conversation's lock) |
| `MAX_RESULT_CHARS` | 100,000 | a tool result; longer results are cut, with a line saying so |
| `RETRY_AFTER_FAILURE` | 5 min | after a failed connect, model calls skip the server (`FAILED_AT`) instead of waiting on it every turn |

The skip applies to model calls only (`Attempt::SkipRecentFailures`). The status check uses `Attempt::Always`: the user asked, so it always tries. A successful connect, or an `evict`, clears the failure.

## Tools

Tools are named `mcp__<server>__<tool>`. `parse_tool_name` splits on the first `__` after the prefix, so a tool name with underscores of its own still resolves.

- **Listing.** `anthropic::tools::tool_definitions` appends `mcp::tool_definitions_for` to smelt's native tools for every request (and the context detail view). It connects to all servers at once, each spawned and given `TOOL_LIST_WAIT`. A server that can't be reached is left out of this call and logged; the turn goes on without its tools. The system prompt's environment section also names the configured servers (see [api.md](api.md#sending-a-message-send_message)).
- **Calling.** `anthropic::tools::execute` checks `parse_tool_name` before the native tools. `call_mcp_tool` looks the server up by name (a deleted server is a plain tool error) and calls `mcp::call_tool`. Arguments must be a JSON object (or null). Text content blocks are joined with newlines; any other block (image, resource) is JSON-encoded rather than dropped. A result with `is_error` comes back as `Err`, so the model sees a failed tool call.

## Status check

`mcp_server_status` backs the badge on `/mcp-servers` and the full status on a server's edit page. It's a real connection attempt (`mcp::connection_check`), not a cached guess, and returns `Connected { tool_names }`, `Unreachable { error }`, or `NotConnected` for an OAuth server with no credentials yet. It shares the registry, so a server already connected is reported without another round trip.

## OAuth

`rmcp::transport::auth` implements the MCP OAuth client: discovery (RFC 9728/8414), dynamic client registration, PKCE and refresh. `src/mcp_oauth.rs` adds:

- **`PgCredentialStore`**, a `CredentialStore` for one server id that loads, saves and clears `mcp_servers.oauth_credentials`, so a grant survives a restart (`rmcp`'s default store is in memory). It takes its pool explicitly, so tests point it at a `#[sqlx::test]` database.
- **Start.** `start_mcp_server_oauth` builds the redirect URI `{base}/oauth/mcp-callback/{id}`. `mcp_oauth::request_base_url` takes `base` from `SMELT_BASE_URL` if set, otherwise from the request's `Host` header, with `X-Forwarded-Proto` for the scheme (default `http`). `mcp_oauth::start` creates an `AuthorizationManager` with the `PgCredentialStore`, starts authorization and returns the provider's URL; the browser navigates there itself. With `oauth_client_id` set, it uses that pre-registered client (plus `oauth_client_secret`, if set) instead of dynamic client registration. GitHub needs this: it publishes no discovery metadata and refuses registration. The attempt (its `OAuthState` and state store: PKCE verifier, CSRF token, expected issuer) waits in the in-memory `PENDING`, one per server; starting again replaces it, and a restart mid-login means starting over.
- **Callback.** `GET /oauth/mcp-callback/{id}` is a plain Axum route (`callback_handler`, see [api.md](api.md#routes-that-arent-server-functions)), since it has to answer with a redirect. It takes the pending attempt and passes `code`, `state` and `iss` to `rmcp`'s `handle_callback_with_issuer`, which exchanges the code and saves the tokens through the store; then it evicts the server's connection. On success it redirects to `/mcp-servers/{id}`, otherwise to `/mcp-servers/{id}?oauth_error=…`, which the edit page shows.
- **Issuer check (RFC 9207, MCP's SEP-2468; SME-65).** When the provider's metadata named an issuer, a callback's `iss` must equal it, and a provider that says it sends `iss` (GitHub and Linear do) must send it. `rmcp` does this for a code callback. An error callback (`error`, e.g. the user denied consent) never reaches `rmcp`, so `verify_error_callback` checks it the same way: the `state` must be the pending attempt's and `issuer_accepted` copies `rmcp`'s rule. Anyone can send an error callback, so until it passes, its text is replaced with `UNVERIFIED_ERROR`. Either way the attempt ends. Separately, `rmcp` refuses metadata whose `issuer` names a different server (RFC 8414 §3.3).
- **Using the token.** For an `oauth` server, `mcp::connect` calls `oauth_headers`: a fresh `AuthorizationManager` on the same store gets an access token, refreshing an expired one and saving the result, and it's sent as `Authorization: Bearer …` alongside `extra_headers`, down the same header path as static mode. A token that expires while a cached connection is open isn't refreshed; the next fresh connect gets a new one.
- **Disconnect.** `disconnect_mcp_server_oauth` clears the stored credentials (the row stays) and evicts the connection.

## Tests

- `src/mcp.rs`: `MockMcpServer`, an in-process `rmcp` server over `tokio::io::duplex`, is registered straight into `REGISTRY` to test naming, dispatch, `tools/list_changed`, the call timeout and the size cap. The Streamable HTTP wire format is `rmcp`'s own responsibility. `connect` itself runs against local TCP servers that hang or hang up, for the connect and listing timeouts, the skip window and the status check retrying anyway. `test_ensure_default_servers_adds_exa_search` is a `#[sqlx::test]`.
- `test_live_exa_search_through_smelt_mcp_client` connects to the real Exa service. It's `#[ignore]`d and also skips unless `SMELT_LIVE_EXA=1`; see [testing.md](testing.md#running-tests).
- `src/mcp_oauth.rs`: `#[sqlx::test]`s against a mock OAuth provider (Axum: `/register`, `/token`, and RFC 8414 metadata only when a test asks for it). They cover the whole start → callback → refresh flow, disconnect, and each issuer case: matching, missing, different, not advertised, error callbacks verified or not, and metadata naming another issuer. `/authorize` is never visited; the tests call the callback directly.
- `src/api/mcp.rs`: `McpServerSummary` carries header names but not values.
