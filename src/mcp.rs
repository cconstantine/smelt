//! Thin wrapper around [`rmcp`](https://crates.io/crates/rmcp) (the
//! official Rust MCP SDK) for smelt's externally-configured MCP servers —
//! see SME-15. `rmcp` owns the actual wire
//! protocol (JSON-RPC framing, Streamable HTTP's two response modes,
//! session tracking, notification dispatch); this module owns the
//! connection registry keyed by `mcp_servers.id`, the
//! `mcp__<server_name>__<tool_name>` naming translation, smelt's
//! `ClientHandler` for server-initiated notifications, and turning an MCP
//! tool-call result into a smelt `ToolResult` string.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolRequestParams, ContentBlock as McpContentBlock, Tool as McpTool};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio::sync::Mutex as AsyncMutex;

use crate::anthropic::ToolDefinition;
use crate::db::McpServerConfig;

/// Exa's hosted MCP server, keyless ("free rate-limited usage without
/// sign-in or API key"), limited to its search tool so reading a page stays
/// with `webfetch`/`http_request`. Adding an `x-api-key` header to the entry
/// on `/mcp-servers` lifts the free limits. See
/// SME-24.
pub const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp?tools=web_search_exa";

/// The MCP servers smelt ships with: `(name, url)`.
const DEFAULT_MCP_SERVERS: &[(&str, &str)] = &[("exa", EXA_MCP_URL)];

/// Adds each of `DEFAULT_MCP_SERVERS` that isn't configured yet, by name —
/// run at startup, so one the user deleted comes back and one they edited
/// is left as they left it. A failure is logged, not fatal: these are
/// extras the rest of smelt doesn't depend on.
pub async fn ensure_default_servers(pool: &sqlx::PgPool) {
    for (name, url) in DEFAULT_MCP_SERVERS {
        match crate::db::ensure_mcp_server(pool, name, url).await {
            Ok(true) => tracing::info!(server = name, "added built-in MCP server"),
            Ok(false) => {}
            Err(e) => tracing::warn!(server = name, error = %e, "couldn't add built-in MCP server"),
        }
    }
}

/// How long listing tools for a model call waits for a server that isn't
/// connected yet. A healthy server connects in a second or two; one that
/// doesn't answer is left out of this call rather than holding it up.
const TOOL_LIST_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(1)
} else {
    std::time::Duration::from_secs(10)
};

/// How long connecting to a server (and listing its tools) may take, for a
/// turn or the status page alike. Without it an unreachable host waited
/// out the operating system's own connect timeout, minutes (SME-51 B8).
const CONNECT_TIMEOUT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(1)
} else {
    std::time::Duration::from_secs(15)
};

/// How long one tool call may take. A call holds the turn, and the
/// conversation's lock, while it waits (SME-51 B8).
const CALL_TIMEOUT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(1)
} else {
    std::time::Duration::from_secs(120)
};

/// After a failed connection, model calls skip the server for this long
/// instead of trying (and waiting) again on every call. The status page
/// still tries each time it's opened.
const RETRY_AFTER_FAILURE: std::time::Duration = std::time::Duration::from_secs(300);

const TOOL_NAME_PREFIX: &str = "mcp__";
const TOOL_NAME_SEPARATOR: &str = "__";

fn namespaced_tool_name(server_name: &str, tool_name: &str) -> String {
    format!("{TOOL_NAME_PREFIX}{server_name}{TOOL_NAME_SEPARATOR}{tool_name}")
}

/// Given a full smelt-facing tool name, returns `(server_name, tool_name)`
/// if it's an MCP-dispatched tool (`mcp__<server>__<tool>`) — `None`
/// otherwise, so `anthropic::tools::execute`'s dispatcher can fall through
/// to smelt's native tools. Splits on the *first* remaining `__` only, so
/// a tool name that itself contains further underscores (e.g.
/// `mcp__github__list_issues`) still resolves correctly.
pub fn parse_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(TOOL_NAME_PREFIX)?;
    rest.split_once(TOOL_NAME_SEPARATOR)
}

/// Refuses extra headers an `auth_mode` server can't use: an OAuth
/// server's `Authorization` is its token, which rmcp's `AuthClient` sets (SME-76 lets OAuth servers have other headers, such as
/// GitHub's `X-MCP-Toolsets`).
pub fn check_extra_headers(auth_mode: &str, headers: &HashMap<String, String>) -> Result<(), String> {
    if auth_mode == "oauth" && headers.keys().any(|name| name.eq_ignore_ascii_case("authorization")) {
        return Err(
            "an OAuth server's Authorization header is set by OAuth; connect it instead".to_string(),
        );
    }
    Ok(())
}

/// Parses `extra_headers` into the `http` crate's typed header map
/// `rmcp`'s transport config expects. A per-server config error (an
/// invalid header name/value the UI let through) surfaces as a plain
/// `Err` rather than a panic.
fn build_header_map(
    extra_headers: &HashMap<String, String>,
) -> Result<HashMap<http::HeaderName, http::HeaderValue>, String> {
    extra_headers
        .iter()
        .map(|(name, value)| {
            let header_name = http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| format!("invalid header name {name:?}: {e}"))?;
            let header_value = http::HeaderValue::from_str(value)
                .map_err(|e| format!("invalid header value for {name:?}: {e}"))?;
            Ok((header_name, header_value))
        })
        .collect()
}

/// smelt's `ClientHandler` implementation — the extension point `rmcp`
/// calls into for server-initiated notifications. Only `tools/list_changed`
/// drives real behavior in this pass: it marks the connection's cached
/// tool list stale so the next call to `tool_definitions_for` refreshes it,
/// rather than waiting for someone to edit the server's config. Everything
/// else `ClientHandler` can report (progress, resource/prompt list
/// changes, logging messages, ...) uses the trait's own no-op defaults —
/// accepted, not acted on; see SME-15's "Server-initiated notifications."
#[derive(Clone)]
struct SmeltClientHandler {
    stale: Arc<AtomicBool>,
}

impl ClientHandler for SmeltClientHandler {
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        self.stale.store(true, Ordering::SeqCst);
        std::future::ready(())
    }
}

struct Connection {
    // Shared so a tool call can run without holding `REGISTRY`'s lock.
    service: Arc<RunningService<RoleClient, SmeltClientHandler>>,
    tools: Vec<McpTool>,
    stale: Arc<AtomicBool>,
}

/// One entry per configured server (`mcp_servers.id`), lazily connected on
/// first use. Not persisted — re-derivable at any time via a fresh
/// `initialize` handshake, the same reasoning `sandbox.rs`'s per-pod
/// connection registry uses. `update_mcp_server`/`delete_mcp_server`
/// (`src/api/mcp.rs`) call `evict` to drop a row's stale entry when its
/// config changes. An entry is only ever a connection built from the row
/// as it was after the server's last `evict` (SME-167).
static REGISTRY: LazyLock<AsyncMutex<HashMap<i64, Connection>>> =
    LazyLock::new(|| AsyncMutex::new(HashMap::new()));

/// How many times each server has been evicted. A connect records it
/// before reading the row and keeps its result only if it hasn't moved
/// (SME-167). Read and written only under `REGISTRY`'s guard, which
/// `generation` takes to say so, so a check and the write it guards are
/// one step.
static GENERATION: LazyLock<std::sync::Mutex<HashMap<i64, u64>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn lock_generations() -> std::sync::MutexGuard<'static, HashMap<i64, u64>> {
    GENERATION.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn generation(_registry: &HashMap<i64, Connection>, server_id: i64) -> u64 {
    lock_generations().get(&server_id).copied().unwrap_or(0)
}

/// Drops a server's cached connection, if any — called after an edit,
/// a delete, a Disconnect or a new sign-in, so a stale connection (old
/// URL, old headers, old grant) is never reused. A connect already in
/// flight is discarded when it finishes (SME-167). A future
/// `tool_definitions_for`/`call_tool` call reconnects from the row.
pub async fn evict(server_id: i64) {
    {
        let mut registry = REGISTRY.lock().await;
        registry.remove(&server_id);
        *lock_generations().entry(server_id).or_default() += 1;
        lock_failures().remove(&server_id);
    }
    OAUTH_CLIENTS.lock().await.remove(&server_id);
}

/// An OAuth server's HTTP client: rmcp's `AuthClient`, which gets a token
/// from the server's `AuthorizationManager` before every request.
type OAuthClient = rmcp::transport::auth::AuthClient<reqwest_rmcp::Client>;

struct SharedOAuthClient {
    client: OAuthClient,
    /// What the manager was built for: the server's URL and the stored
    /// grant's client id. A manager built for anything else isn't reused.
    url: String,
    grant_client_id: Option<String>,
}

/// One `AuthClient` per OAuth server, cloned into each of its connections
/// and used by the status check (SME-113). Clones share one manager behind
/// one mutex, so refreshes are one at a time: with GitHub rotating refresh
/// tokens on every refresh, two managers refreshing at once would retire
/// each other's tokens. `evict` drops the entry (an edit, a delete, a
/// Disconnect, a new grant), and an entry built for another URL or another
/// grant's client is rebuilt rather than reused, which also covers a build
/// that finishes after an `evict`.
static OAUTH_CLIENTS: LazyLock<AsyncMutex<HashMap<i64, SharedOAuthClient>>> =
    LazyLock::new(|| AsyncMutex::new(HashMap::new()));

/// One lock per server, held while building its `AuthClient`, so two
/// callers don't build two managers.
static BUILDING_OAUTH: LazyLock<std::sync::Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn grant_client_id(config: &McpServerConfig) -> Option<String> {
    config
        .oauth_credentials
        .as_ref()
        .and_then(|json| json.0.get("client_id")?.as_str().map(str::to_string))
}

/// The server's shared `AuthClient`, built on first use.
async fn oauth_client(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
) -> Result<OAuthClient, String> {
    let grant_client_id = grant_client_id(config);
    let lock = server_lock(&BUILDING_OAUTH, config.id);
    let _building = lock.lock().await;
    if let Some(shared) = OAUTH_CLIENTS.lock().await.get(&config.id)
        && shared.url == config.url
        && shared.grant_client_id == grant_client_id
    {
        return Ok(shared.client.clone());
    }
    let manager = crate::mcp_oauth::connection_manager(pool, config).await?;
    let client = OAuthClient::new(transport_http_client()?, manager);
    OAUTH_CLIENTS.lock().await.insert(
        config.id,
        SharedOAuthClient {
            client: client.clone(),
            url: config.url.clone(),
            grant_client_id,
        },
    );
    Ok(client)
}

/// Drops `server_id`'s cached connection if it's still `service`: one a
/// caller found dead, not a newer one that replaced it meanwhile.
async fn drop_connection(
    server_id: i64,
    service: &Arc<RunningService<RoleClient, SmeltClientHandler>>,
) {
    let mut registry = REGISTRY.lock().await;
    if registry
        .get(&server_id)
        .is_some_and(|conn| Arc::ptr_eq(&conn.service, service))
    {
        registry.remove(&server_id);
    }
}

/// When each server last failed to connect, so model calls can skip it
/// for `RETRY_AFTER_FAILURE` instead of waiting on it every time.
static FAILED_AT: LazyLock<std::sync::Mutex<HashMap<i64, std::time::Instant>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn lock_failures() -> std::sync::MutexGuard<'static, HashMap<i64, std::time::Instant>> {
    FAILED_AT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn failed_recently(server_id: i64) -> bool {
    lock_failures()
        .get(&server_id)
        .is_some_and(|at| at.elapsed() < RETRY_AFTER_FAILURE)
}

/// One lock per server, held while connecting to it, so two callers don't
/// both connect to the same server, and a slow server only holds up
/// callers that want that server, not `REGISTRY` as a whole.
static CONNECTING: LazyLock<std::sync::Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn server_lock(
    locks: &std::sync::Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>,
    server_id: i64,
) -> Arc<AsyncMutex<()>> {
    locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(server_id)
        .or_default()
        .clone()
}

/// Whether `ensure_connected` should try a server that failed recently.
#[derive(Clone, Copy, PartialEq)]
enum Attempt {
    /// Model calls: skip a server that failed within `RETRY_AFTER_FAILURE`.
    SkipRecentFailures,
    /// The status page: the user is asking, so always try.
    Always,
}

/// Calls `f` and, if it fails, retries exactly once. A server's very first
/// connection attempt is the one most likely to hit a one-off transient
/// failure (a slow DNS lookup, a dropped packet, a dev-server rebuild
/// racing the request — see SME-15's
/// retrospective). Generic over `f`'s return type so it's unit-testable
/// without a real network call; `connect`'s only caller wires it in below.
async fn retry_once<F, Fut, T, E>(mut f: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    match f().await {
        Ok(value) => Ok(value),
        Err(_first_error) => f().await,
    }
}

/// Installs ring as rustls's process-wide default provider, if none is yet.
/// rmcp's reqwest client is built with `rustls-no-provider` (one crypto
/// backend in the binary, SME-60), and building it with no default installed
/// panics. `main` installs ring at startup; calling this where rmcp builds a
/// client keeps the MCP paths safe without depending on that order.
pub(crate) fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The HTTP client rmcp's own transport builds for itself
/// (`default_http_client`, private): no idle pooling, and no redirects, so
/// a header (an OAuth token included) is never replayed to a redirect
/// target.
fn transport_http_client() -> Result<reqwest_rmcp::Client, String> {
    reqwest_rmcp::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest_rmcp::redirect::Policy::none())
        .build()
        .map_err(|e| format!("failed to build the MCP HTTP client: {e}"))
}

type Serving = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<
                    RunningService<RoleClient, SmeltClientHandler>,
                    rmcp::service::ClientInitializeError,
                >,
            > + Send,
    >,
>;

/// Why connecting failed. `sign_in_rejected`: the server answered 401
/// even after `AuthClient`'s refresh, or no token could be had, so only a
/// Reconnect will help (SME-113).
#[derive(Debug)]
struct ConnectError {
    message: String,
    sign_in_rejected: bool,
}

impl From<String> for ConnectError {
    fn from(message: String) -> Self {
        Self {
            message,
            sign_in_rejected: false,
        }
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Whether a transport error is the server's 401, which `AuthClient`
/// passes on once its refresh and retry haven't helped.
fn is_auth_rejection(error: &rmcp::transport::DynamicTransportError) -> bool {
    use rmcp::transport::streamable_http_client::StreamableHttpError;
    error
        .error
        .downcast_ref::<StreamableHttpError<reqwest_rmcp::Error>>()
        .is_some_and(|e| {
            matches!(
                e,
                StreamableHttpError::AuthRequired(_)
                    | StreamableHttpError::Auth(rmcp::transport::auth::AuthError::AuthorizationRequired)
            )
        })
}

/// Connects to `config`'s server in an `mcp connect` span (SME-137), marked
/// failed when it can't.
async fn connect(pool: &sqlx::PgPool, config: &McpServerConfig) -> Result<Connection, ConnectError> {
    use tracing::field::Empty;
    let span = tracing::info_span!(
        "mcp connect",
        smelt.mcp.server = %config.name,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        let result = connect_unspanned(pool, config).await;
        if let Err(e) = &result {
            crate::telemetry::mark_error(&span, &e.message);
        }
        result
    })
    .await
}

async fn connect_unspanned(pool: &sqlx::PgPool, config: &McpServerConfig) -> Result<Connection, ConnectError> {
    install_crypto_provider();
    let headers = build_header_map(&config.extra_headers.0)?;
    let transport_config =
        StreamableHttpClientTransportConfig::with_uri(config.url.clone()).custom_headers(headers);
    let http_client = transport_http_client()?;

    let stale = Arc::new(AtomicBool::new(false));
    let handler = SmeltClientHandler {
        stale: stale.clone(),
    };
    let name = config.name.clone();
    let (pool, config) = (pool.clone(), config.clone());
    let connecting = async move {
        // An OAuth server's requests go through rmcp's `AuthClient`, which
        // gets a token before each one (refreshing it when it's expiring)
        // and, when the server rejects one anyway, refreshes once and
        // retries (SME-113). A token baked into the connection's headers
        // expired with the connection still cached. Building the manager
        // runs the provider's metadata discovery, so it's inside the
        // timeout too.
        let serving: Serving = if config.auth_mode == "oauth" {
            let client = oauth_client(&pool, &config).await?;
            Box::pin(handler.serve(StreamableHttpClientTransport::with_client(client, transport_config)))
        } else {
            Box::pin(handler.serve(StreamableHttpClientTransport::with_client(http_client, transport_config)))
        };
        let service = serving.await.map_err(|e| ConnectError {
            sign_in_rejected: matches!(
                &e,
                rmcp::service::ClientInitializeError::TransportError { error, .. } if is_auth_rejection(error)
            ),
            message: format!("failed to connect to MCP server {:?}: {e}", config.name),
        })?;
        let tools = service.list_all_tools().await.map_err(|e| {
            format!(
                "failed to list tools from MCP server {:?}: {e}",
                config.name
            )
        })?;
        Ok::<_, ConnectError>((service, tools))
    };
    // Outside every span: rmcp's service loop keeps `Span::current()` for
    // as long as the connection is cached, which would hold the turn that
    // first connected, and its request, open (SME-137).
    let (service, tools) = crate::telemetry::outside_spans(tokio::time::timeout(CONNECT_TIMEOUT, connecting))
        .await
        .map_err(|_| {
            ConnectError::from(format!(
                "MCP server {:?} didn't answer within {}s",
                name,
                CONNECT_TIMEOUT.as_secs()
            ))
        })??;

    Ok(Connection {
        service: Arc::new(service),
        tools,
        stale,
    })
}

/// Ensures `REGISTRY` has a live, up-to-date entry for `config.id` —
/// connecting for the first time, or just re-listing tools if the existing
/// connection's cached list went stale (a `tools/list_changed`
/// notification fired). A `list_all_tools` failure on an existing
/// connection is treated as "the connection is broken," not just "the list
/// is stale": the entry is dropped so the fallback below reconnects fully.
async fn ensure_connected(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    attempt: Attempt,
) -> Result<(), ConnectError> {
    let existing = {
        let registry = REGISTRY.lock().await;
        registry
            .get(&config.id)
            .map(|conn| (conn.service.clone(), conn.stale.swap(false, Ordering::SeqCst)))
    };
    if let Some((service, stale)) = existing {
        if !stale {
            return Ok(());
        }
        match service.list_all_tools().await {
            Ok(tools) => {
                if let Some(conn) = REGISTRY.lock().await.get_mut(&config.id) {
                    conn.tools = tools;
                }
                return Ok(());
            }
            Err(_) => drop_connection(config.id, &service).await,
        }
    }

    let skip_error = || {
        format!(
            "MCP server {:?} failed to connect recently; model calls try it again after {} minutes",
            config.name,
            RETRY_AFTER_FAILURE.as_secs() / 60
        )
    };
    if attempt == Attempt::SkipRecentFailures && failed_recently(config.id) {
        return Err(skip_error().into());
    }
    let lock = server_lock(&CONNECTING, config.id);
    let _connecting = lock.lock().await;
    // Someone else may have connected, or failed, while this waited.
    let started = {
        let registry = REGISTRY.lock().await;
        if registry.contains_key(&config.id) {
            return Ok(());
        }
        generation(&registry, config.id)
    };
    if attempt == Attempt::SkipRecentFailures && failed_recently(config.id) {
        return Err(skip_error().into());
    }
    // The row as it is now, not the caller's copy, which may predate an
    // edit whose `evict` came before `started` (SME-167). An edit that
    // commits after this read evicts after it too, which the check
    // below sees, or which removes the entry.
    let row = match crate::db::get_mcp_server_config(pool, config.id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(format!("MCP server {:?} no longer exists", config.name).into()),
        Err(e) => return Err(format!("couldn't read MCP server {:?}'s settings: {e}", config.name).into()),
    };
    let result = retry_once(|| connect(pool, &row)).await;
    let mut registry = REGISTRY.lock().await;
    if generation(&registry, config.id) != started {
        // Evicted while connecting: nothing is kept or recorded, and the
        // connection (if any) is dropped, which closes it.
        return Err(format!(
            "MCP server {:?}'s settings changed while connecting; it connects again on the next call",
            config.name
        )
        .into());
    }
    match result {
        Ok(connection) => {
            lock_failures().remove(&config.id);
            registry.insert(config.id, connection);
            Ok(())
        }
        Err(e) => {
            lock_failures().insert(config.id, std::time::Instant::now());
            Err(e)
        }
    }
}

fn mcp_tool_to_definition(server_name: &str, tool: &McpTool) -> ToolDefinition {
    ToolDefinition {
        name: namespaced_tool_name(server_name, tool.name.as_ref()),
        description: tool.description.clone().unwrap_or_default().into_owned(),
        input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// Builds the namespaced `ToolDefinition`s for every configured server —
/// `anthropic::tools::tool_definitions` appends these to smelt's own
/// static tool list on every `send_message` call. A server that fails to
/// connect this turn is skipped (logged, not fatal) rather than failing
/// the whole tool list — the model just doesn't see that server's tools
/// until it's reachable again.
pub async fn tool_definitions_for(
    pool: &sqlx::PgPool,
    configs: &[McpServerConfig],
) -> Vec<ToolDefinition> {
    // All servers at once, each given at most `TOOL_LIST_WAIT`. A connect
    // that takes longer carries on in the background (it's spawned, so
    // giving up on it here doesn't cancel it), and its tools show up on a
    // later call once it's connected.
    let attempts = configs.iter().map(|config| {
        let (pool, config) = (pool.clone(), config.clone());
        let name = config.name.clone();
        // In the caller's span, so its connect is the turn's child (SME-137).
        let task = tokio::spawn(tracing::Instrument::in_current_span(async move {
            ensure_connected(&pool, &config, Attempt::SkipRecentFailures).await
        }));
        async move {
            match tokio::time::timeout(TOOL_LIST_WAIT, task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    // Its URL's query can hold the server's key, and this
                    // line is exported with the turn's span (SME-137).
                    let error = crate::telemetry::scrub_urls(&e.to_string());
                    tracing::warn!(server = %name, %error, "MCP server unreachable this turn; its tools are unavailable");
                }
                Ok(Err(e)) => {
                    tracing::warn!(server = %name, error = %e, "MCP connect task failed");
                }
                Err(_) => {
                    tracing::warn!(server = %name, "MCP server still connecting; its tools are left out of this call");
                }
            }
        }
    });
    futures_util::future::join_all(attempts).await;

    let registry = REGISTRY.lock().await;
    configs
        .iter()
        .filter_map(|config| registry.get(&config.id).map(|conn| (config, conn)))
        .flat_map(|(config, conn)| {
            conn.tools
                .iter()
                .map(|tool| mcp_tool_to_definition(&config.name, tool))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Why `connection_check` couldn't report a server as connected.
#[derive(Debug, PartialEq)]
pub enum CheckError {
    /// The server, or its OAuth provider, couldn't be reached or failed.
    Unreachable(String),
    /// An OAuth server's sign-in can't be refreshed (no refresh token, or
    /// the provider refused it): only a Reconnect brings it back (SME-113).
    SignInExpired(String),
}

fn sign_in_expired(config: &McpServerConfig) -> String {
    format!(
        "the sign-in to MCP server {:?} has expired or was revoked; use Reconnect",
        config.name
    )
}

/// For an OAuth server, gets a token from its shared manager: a store read,
/// or a refresh when it's expiring. A grant that can't be refreshed drops
/// the server's cached connection, which would otherwise still report
/// Connected (SME-113).
/// The server's stored grant as it is now, to tell whether another
/// refresh replaced it. `None` too when it can't be read.
async fn stored_grant(pool: &sqlx::PgPool, server_id: i64) -> Option<serde_json::Value> {
    crate::db::get_mcp_server_config(pool, server_id)
        .await
        .ok()
        .flatten()
        .and_then(|config| config.oauth_credentials)
        .map(|json| json.0)
}

async fn check_sign_in(pool: &sqlx::PgPool, config: &McpServerConfig) -> Result<(), CheckError> {
    let token = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let client = oauth_client(pool, config)
            .await
            .map_err(CheckError::Unreachable)?;
        // In a task of its own: a refresh the provider has received
        // rotates its refresh token whether or not anyone waits for the
        // reply, so the reply must still be saved when this check times
        // out or its request goes away (review 2).
        let fetch = |client: OAuthClient| async move {
            tokio::spawn(async move { client.get_access_token().await })
                .await
                .map_err(|e| {
                    CheckError::Unreachable(format!(
                        "getting an OAuth token for MCP server {:?} failed: {e}",
                        config.name
                    ))
                })
        };
        let grant_before = stored_grant(pool, config.id).await;
        let mut fetched = fetch(client.clone()).await?;
        if matches!(fetched, Err(rmcp::transport::auth::AuthError::AuthorizationRequired))
            && stored_grant(pool, config.id).await != grant_before
        {
            // The provider refused this refresh, but the grant changed
            // meanwhile: another manager refreshed first with the same
            // refresh token (review 2). Its new grant is what counts.
            fetched = fetch(client).await?;
        }
        fetched.map_err(|e| match e {
            rmcp::transport::auth::AuthError::AuthorizationRequired => {
                CheckError::SignInExpired(sign_in_expired(config))
            }
            other => CheckError::Unreachable(format!(
                "couldn't get an OAuth token for MCP server {:?}: {other}",
                config.name
            )),
        })
    })
    .await
    .map_err(|_| {
        CheckError::Unreachable(format!(
            "MCP server {:?}'s OAuth provider didn't answer within {}s",
            config.name,
            CONNECT_TIMEOUT.as_secs()
        ))
    })?;
    if let Err(CheckError::SignInExpired(_)) = &token {
        evict(config.id).await;
    }
    token.map(|_| ())
}

/// Attempts a real connection to `config`'s server and reports its current
/// tool names on success — the `/mcp-servers` UI's live "connected" status
/// (index page's badge, edit page's full status) rather than a cached
/// guess, since a stale "looks fine" indicator would be worse than no
/// indicator at all. Shares `ensure_connected`'s registry/retry logic with
/// `tool_definitions_for`/`call_tool`, so a server already connected this
/// turn is reported without a second round-trip.
pub async fn connection_check(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
) -> Result<Vec<String>, CheckError> {
    use tracing::field::Empty;
    let span = tracing::info_span!(
        "mcp connection_check",
        smelt.mcp.server = %config.name,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        let result = check_connection(pool, config).await;
        match &result {
            Err(CheckError::Unreachable(message) | CheckError::SignInExpired(message)) => {
                crate::telemetry::mark_error(&span, message)
            }
            Ok(_) => {}
        }
        result
    })
    .await
}

/// `connection_check`'s work, in its span.
async fn check_connection(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
) -> Result<Vec<String>, CheckError> {
    if config.auth_mode == "oauth" {
        check_sign_in(pool, config).await?;
    }
    if let Err(e) = ensure_connected(pool, config, Attempt::Always).await {
        if !e.sign_in_rejected {
            return Err(CheckError::Unreachable(e.message));
        }
        // As `check_sign_in` does for a refusal it sees itself.
        evict(config.id).await;
        return Err(CheckError::SignInExpired(sign_in_expired(config)));
    }
    let registry = REGISTRY.lock().await;
    let conn = registry.get(&config.id).ok_or_else(|| {
        CheckError::Unreachable(format!(
            "connection to MCP server {:?} vanished immediately after connecting",
            config.name
        ))
    })?;
    Ok(conn
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect())
}

/// Dispatches a `tools/call` to `config`'s server and turns the result
/// into a plain string — `content[].text` blocks concatenated; any other
/// content block type (image, resource, ...) is JSON-encoded inline rather
/// than dropped, matching the same "don't lose it" fallback other tool
/// dispatch paths in this codebase already use. An `is_error` result comes
/// back as `Err` so `anthropic::tools::execute` wraps it in a
/// `ContentBlock::ToolResult { is_error: Some(true), .. }` like any other
/// failed tool call.
pub async fn call_tool(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<String, String> {
    use tracing::field::Empty;
    // The server and tool, never the arguments or result (SME-137).
    let span = tracing::info_span!(
        "mcp call",
        smelt.mcp.server = %config.name,
        smelt.mcp.tool = %tool_name,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        call_tool_unspanned(pool, config, tool_name, arguments).await.map_err(|e| match e {
            CallFailure::Reported(message) => {
                crate::telemetry::mark_error(&span, CallFailure::REPORTED);
                message
            }
            CallFailure::Failed(message) => {
                crate::telemetry::mark_error(&span, &message);
                message
            }
            CallFailure::Transport { message, description } => {
                crate::telemetry::mark_error(&span, &description);
                message
            }
        })
    })
    .await
}

/// How a `tools/call` failed: the tool said so, in output of its own (or
/// smelt refused the call's input, quoting it), or the call itself did.
/// Only the second's message goes on the span: a span never holds tool
/// output or input (SME-137 review 1).
enum CallFailure {
    Reported(String),
    Failed(String),
    /// Below MCP: the span gets `description`, the model `message`.
    Transport { message: String, description: String },
}

/// What a `tools/call` that failed below MCP says on its span: the tool,
/// the server and the kind of failure, never the error's text, which can
/// quote the server's reply (SME-137 review 2).
fn transport_failure_description(tool_name: &str, server: &str, error: &rmcp::service::ServiceError) -> String {
    use rmcp::service::ServiceError;
    let kind = match error {
        ServiceError::McpError(_) => "the server returned an error",
        ServiceError::TransportSend(_) => "a transport error (the server's reply is in the conversation)",
        ServiceError::TransportClosed => "the transport closed",
        ServiceError::UnexpectedResponse => "an unexpected response",
        ServiceError::Timeout { .. } => "rmcp's request timeout",
        ServiceError::Cancelled { .. } => "the request was cancelled",
        _ => "an rmcp error",
    };
    format!("MCP tool call to {tool_name:?} on {server:?} failed: {kind}")
}

impl CallFailure {
    const REPORTED: &'static str = "the MCP tool reported an error (its output is in the conversation)";
}

async fn call_tool_unspanned(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<String, CallFailure> {
    let arguments = match arguments {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => None,
        other => {
            return Err(CallFailure::Reported(format!(
                "tool arguments must be a JSON object, got: {other}"
            )));
        }
    };

    ensure_connected(pool, config, Attempt::SkipRecentFailures)
        .await
        .map_err(|e| CallFailure::Failed(e.message))?;
    let service = REGISTRY
        .lock()
        .await
        .get(&config.id)
        .map(|conn| conn.service.clone())
        .ok_or_else(|| CallFailure::Failed(format!("not connected to MCP server {:?}", config.name)))?;

    let mut request = CallToolRequestParams::new(tool_name.to_string());
    if let Some(arguments) = arguments {
        request = request.with_arguments(arguments);
    }

    let result = tokio::time::timeout(CALL_TIMEOUT, service.call_tool(request))
        .await
        .map_err(|_| {
            CallFailure::Failed(format!(
                "MCP server {:?} didn't answer {tool_name:?} within {}s",
                config.name,
                CALL_TIMEOUT.as_secs()
            ))
        })?;
    let result = match result {
        Ok(result) => result,
        Err(e) => {
            // Below MCP (a transport error, such as a 401 that survived
            // `AuthClient`'s refresh and retry, or a closed connection),
            // the connection is taken as dead, so the next caller connects
            // again rather than every call failing on it (SME-113). A
            // JSON-RPC error from the tool itself isn't the connection's.
            let from_the_tool = matches!(e, rmcp::service::ServiceError::McpError(_));
            if !from_the_tool {
                drop_connection(config.id, &service).await;
            }
            let message = format!("MCP tool call to {:?} on {:?} failed: {e}", tool_name, config.name);
            // The server's own error text, or its reply quoted in a
            // transport error, can quote the call's input (review 2).
            return Err(if from_the_tool {
                CallFailure::Reported(message)
            } else {
                CallFailure::Transport { description: transport_failure_description(tool_name, &config.name, &e), message }
            });
        }
    };

    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            McpContentBlock::Text(text) => text.text,
            other => serde_json::to_string(&other)
                .unwrap_or_else(|_| "<unrepresentable MCP content block>".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_error.unwrap_or(false) {
        Err(CallFailure::Reported(content))
    } else {
        Ok(content)
    }
}

#[cfg(test)]
mod tests;
