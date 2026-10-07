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
- **Not connected:** connect under a per-server lock (`CONNECTING`), so two callers don't both connect and a slow server holds up only callers that want it, not the whole registry (SME-40). `connect` builds the transport with the headers (an OAuth server's requests also go through rmcp's `AuthClient`, below), runs the `initialize` handshake and lists all tools, all within `CONNECT_TIMEOUT`. `retry_once` tries once more on failure, since a first connect is the one most likely to hit a one-off problem.
- **TLS.** `rmcp`'s HTTP client is reqwest 0.13 built without a crypto provider of its own (`reqwest-tls-no-provider`), so rustls uses the process-wide one: ring, the only backend in the binary (SME-60). reqwest panics if a client is built before one is installed, so `install_crypto_provider` installs ring (idempotently) before `connect`, `mcp_oauth::connection_manager` and `mcp_oauth::start` build one, not relying on `main` having done it first.

Limits (SME-51); the three timeouts are 1s under `cfg(test)`. A tool result's size is capped for every tool, MCP or native, by `anthropic::tools::execute` (`MAX_TOOL_RESULT_CHARS`, SME-76; see [api.md](api.md)).

| Constant | Value | Bounds |
|---|---|---|
| `CONNECT_TIMEOUT` | 15s | connecting plus listing tools, per attempt |
| `TOOL_LIST_WAIT` | 10s | how long a turn waits for a server when listing tools; a connect that takes longer carries on in the background and its tools show up on a later call |
| `CALL_TIMEOUT` | 120s | one tool call (which holds the turn and the conversation's lock) |
| `RETRY_AFTER_FAILURE` | 5 min | after a failed connect, model calls skip the server (`FAILED_AT`) instead of waiting on it every turn |

The skip applies to model calls only (`Attempt::SkipRecentFailures`). The status check uses `Attempt::Always`: the user asked, so it always tries. A successful connect, or an `evict`, clears the failure.

## Tools

Tools are named `mcp__<server>__<tool>`. `parse_tool_name` splits on the first `__` after the prefix, so a tool name with underscores of its own still resolves.

- **Listing.** `anthropic::tools::tool_definitions` appends `mcp::tool_definitions_for` to smelt's native tools for every request (and the context detail view). It connects to all servers at once, each spawned and given `TOOL_LIST_WAIT`. A server that can't be reached is left out of this call and logged; the turn goes on without its tools. The system prompt's environment section also names the configured servers (see [api.md](api.md#sending-a-message-send_message)).
- **Calling.** `anthropic::tools::execute` checks `parse_tool_name` before the native tools. `call_mcp_tool` looks the server up by name (a deleted server is a plain tool error) and calls `mcp::call_tool`. Arguments must be a JSON object (or null). Text content blocks are joined with newlines; any other block (image, resource) is JSON-encoded rather than dropped. A result with `is_error` comes back as `Err`, so the model sees a failed tool call. A call that fails with anything but a JSON-RPC error from the tool (a transport error: a 401 that survived `AuthClient`'s refresh and retry, a closed connection, but also a one-off HTTP 429 or 500; or an rmcp protocol failure such as an unexpected response) also drops the cached connection (`drop_connection`, only if it's still that connection), so the next caller connects again (SME-113); a JSON-RPC error from the tool, or `CALL_TIMEOUT`, keeps it.

## Status check

`mcp_server_status` backs the badge on `/mcp-servers` and the full status on a server's edit page. It's a real connection attempt (`mcp::connection_check`), not a cached guess, and returns `Connected { tool_names }`, `Unreachable { error }`, `NotConnected` for an OAuth server with no credentials yet, or `NeedsReconnect { error }` for one whose sign-in can't be refreshed (SME-113; the list's badge reads "Sign-in expired", in amber, and the edit page offers Reconnect). It shares the registry, so a server already connected is reported without another round trip. For an OAuth server it first asks the server's shared manager for a token (`check_sign_in`: a store read, or a refresh when it's expiring), so an expired sign-in isn't reported as Connected from a cached connection. When that fails with `AuthorizationRequired` (no refresh token, or the provider refused it), or a fresh connect still gets a 401 after `AuthClient`'s refresh, the result is `NeedsReconnect` and the server is evicted. The second case can mislead: `AuthClient` passes the 401 on whenever its refresh fails, for any reason, so a token the server rejects before its expiry while the provider is down also reads as expired. `mcp::connection_check` returns `CheckError::{Unreachable, SignInExpired}`.

## OAuth

`rmcp::transport::auth` implements the MCP OAuth client: discovery (RFC 9728/8414), dynamic client registration, PKCE and refresh. `src/mcp_oauth.rs` adds:

- **`PgCredentialStore`**, a `CredentialStore` for one server id that loads, saves and clears `mcp_servers.oauth_credentials`, so a grant survives a restart (`rmcp`'s default store is in memory). It takes its pool explicitly, so tests point it at a `#[sqlx::test]` database. A login attempt's store (`PgCredentialStore::new`) saves unconditionally. A connection manager's (`for_refreshes`) saves a refresh only while the row still holds a grant for the same client id (`db::save_refreshed_mcp_server_oauth_credentials`), and fails otherwise, so a refresh that finishes after a Disconnect, a URL change or a new Connect doesn't bring the old grant back and the request it was for doesn't go on with it (SME-113). With a pre-registered client the id doesn't change, so a refresh racing a Reconnect can still overwrite the new grant with a valid refreshed old one.
- **Start.** `start_mcp_server_oauth` builds the redirect URI `{base}/oauth/mcp-callback/{id}`. `mcp_oauth::request_base_url` takes `base` from `SMELT_BASE_URL` if set, otherwise from the request's `Host` header, with `X-Forwarded-Proto` for the scheme (default `http`). `mcp_oauth::start` creates an `AuthorizationManager` with the `PgCredentialStore`, starts authorization and returns the provider's URL; the browser navigates there itself. With `oauth_client_id` set, it uses that pre-registered client (plus `oauth_client_secret`, if set) instead of dynamic client registration. GitHub needs this: it publishes no discovery metadata and refuses registration. The attempt (its `OAuthState` and state store: PKCE verifier, CSRF token, expected issuer) waits in the in-memory `PENDING`, one per server; starting again replaces it, and a restart mid-login means starting over.
- **Callback.** `GET /oauth/mcp-callback/{id}` is a plain Axum route (`callback_handler`, see [api.md](api.md#routes-that-arent-server-functions)), since it has to answer with a redirect. It takes the pending attempt and passes `code`, `state` and `iss` to `rmcp`'s `handle_callback_with_issuer`, which exchanges the code and saves the tokens through the store; then it evicts the server's connection. On success it redirects to `/mcp-servers/{id}`, otherwise to `/mcp-servers/{id}?oauth_error=…`, which the edit page shows.
- **Issuer check (RFC 9207, MCP's SEP-2468; SME-65).** When the provider's metadata named an issuer, a callback's `iss` must equal it, and a provider that says it sends `iss` (GitHub and Linear do) must send it. `rmcp` does this for a code callback. An error callback (`error`, e.g. the user denied consent) never reaches `rmcp`, so `verify_error_callback` checks it the same way: the `state` must be the pending attempt's and `issuer_accepted` copies `rmcp`'s rule. Anyone can send an error callback, so until it passes, its text is replaced with `UNVERIFIED_ERROR`. Either way the attempt ends. Separately, `rmcp` refuses metadata whose `issuer` names a different server (RFC 8414 §3.3).
- **Using the token (SME-113).** An `oauth` server's transport is rmcp's `AuthClient` wrapped around a reqwest 0.13 client (`reqwest_rmcp`, the version rmcp uses; smelt's own `reqwest` is 0.12), built like rmcp's own: no redirects, no idle pooling. `AuthClient` asks the server's `AuthorizationManager` for a token before every HTTP request, which refreshes one within 30 s of its expiry and saves the result; when the server rejects a token anyway (revoked, clock skew) it refreshes once and retries that request. So the MCP session outlives its tokens: GitHub's last 8 hours, Linear's about 24. `extra_headers` go alongside it. Each server has one `AuthClient` (`OAUTH_CLIENTS`), shared by its connections and the status check, so refreshes go one at a time behind its manager's mutex; GitHub rotates the refresh token on every refresh, and two managers refreshing at once retired each other's tokens. `evict` drops it, and an entry built for another URL or another grant's client id is rebuilt rather than reused. An `evict` while a call or connect is still running leaves that caller on the old `AuthClient` while the next one builds a new manager, so two can exist for a moment; their refreshes can then collide: the provider refuses the second use of the refresh token, so one request fails, but each reads the grant fresh, so the grant survives. A status check whose refresh lost such a race tries once more if the stored grant changed meanwhile (`check_sign_in`); only if the refusal reaches it before the winner has saved does it still read "Sign-in expired", until the next check. `mcp_oauth::connection_manager` builds the manager from the stored grant and, for a pre-registered client the grant was issued to, configures `oauth_client_secret` too: rmcp restores only the client id, and GitHub refuses a refresh without the secret (`invalid_client`). The manager talks to the provider through `ProviderHttpClient` (rmcp's default behaviour: a 30 s timeout, the redirect policy each request asks for, a 1 MiB reply cap) plus one repair: GitHub refuses a token request with HTTP 200 and an `error` in the body, and names a dead refresh token `bad_refresh_token`. Such a reply to a POST is passed on as the 400 `invalid_grant` RFC 6749 gives it, so rmcp reports `AuthorizationRequired` and the status says Sign-in expired, not Unreachable.
- **Disconnect.** `disconnect_mcp_server_oauth` clears the stored credentials (the row stays) and evicts the connection.
- **Extra headers (SME-76).** An OAuth server's edit page has the same header editor as a static-header server ("Extra headers (sent with the OAuth token)"). `mcp::check_extra_headers` refuses an `Authorization` header on an OAuth server, on create and on update, since `AuthClient` sets it.
- **GitHub's CI logs.** GitHub's hosted server (`https://api.githubcopilot.com/mcp/`) exposes only its default toolsets (`context`, `repos`, `issues`, `pull_requests`, `users`), which can't read Actions logs. Add the header `X-MCP-Toolsets: context,repos,issues,pull_requests,users,actions` to turn on the `actions` toolset too, whose `get_job_logs` reads a job's log (`job_id`, `tail_lines`, `failed_only`). The URL form `/mcp/x/actions` would enable *only* that toolset. See GitHub's [remote server docs](https://github.com/github/github-mcp-server/blob/main/docs/remote-server.md).

## Tests

- `src/mcp.rs`: `oauth_http`, a mock over real HTTP (SME-113): an MCP server answering in JSON that 401s any token but the ones it accepts, plus an OAuth provider at rmcp's default endpoints that rotates refresh tokens like GitHub, can require a client secret, refuse refreshes (`invalid_grant`) or hold one at a gate. Its `#[sqlx::test]`s cover a refresh per request, a revoked token, a restart, a pre-registered client's secret, eviction after a transport failure, refreshes racing a Disconnect or a new grant, one refresh shared by the status check and a call, a new grant's client, and expired sign-ins. `MockMcpServer`, an in-process `rmcp` server over `tokio::io::duplex`, is registered straight into `REGISTRY` to test naming, dispatch, `tools/list_changed`, the call timeout and the size cap. The Streamable HTTP wire format is `rmcp`'s own responsibility. `connect` itself runs against local TCP servers that hang or hang up, for the connect and listing timeouts, the skip window and the status check retrying anyway. `test_ensure_default_servers_adds_exa_search` is a `#[sqlx::test]`.
- `test_live_exa_search_through_smelt_mcp_client` connects to the real Exa service. It's `#[ignore]`d and also skips unless `SMELT_LIVE_EXA=1`; see [testing.md](testing.md#running-tests).
- `src/mcp_oauth.rs`: `#[sqlx::test]`s against a mock OAuth provider (Axum: `/register`, `/token`, and RFC 8414 metadata only when a test asks for it). They cover the whole start → callback → refresh flow, disconnect, and each issuer case: matching, missing, different, not advertised, error callbacks verified or not, and metadata naming another issuer. `/authorize` is never visited; the tests call the callback directly.
- `src/api/mcp.rs`: `McpServerSummary` carries header names but not values.
