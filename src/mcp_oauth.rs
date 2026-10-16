//! OAuth authentication for MCP servers — see
//! SME-16. `rmcp`'s own `auth` feature
//! (`rmcp::transport::auth`) already implements the MCP-spec OAuth client
//! (RFC 9728/8414 discovery, dynamic client registration, PKCE, refresh);
//! this module is the wiring smelt needs around it: a Postgres-backed
//! `CredentialStore` so a connected server's tokens survive a smelt
//! restart (`rmcp`'s own default is in-memory only), and the
//! start/callback/disconnect lifecycle the `/mcp-servers` UI and the
//! `/oauth/mcp-callback/{id}` route (`main.rs`) drive. `src/mcp.rs`'s
//! `connect()` is the other half: its transport's HTTP client is rmcp's
//! `AuthClient`, which gets a token from `connection_manager`'s manager
//! before every request, refreshing it when it's expiring (SME-113).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, InMemoryStateStore,
    OAuthClientConfig, OAuthState, StateStore, StoredAuthorizationState, StoredCredentials,
};
use sqlx::PgPool;
use tokio::sync::Mutex as AsyncMutex;

use crate::db::{self, McpServerConfig};

/// Postgres-backed `CredentialStore` for one server's OAuth grant, bound to
/// a single `mcp_servers.id` and to the URL the manager it's plugged into
/// was built for. Plugged into each `AuthorizationManager` smelt builds
/// (`start`, and `connection_manager` for connections), each with the rule
/// its saves follow (`SaveRule`); it keeps nothing in memory, so every load
/// reads the row fresh. A grant is only ever loaded or saved while the
/// row's URL is still the store's: a token goes only to the address it was
/// issued for (SME-166). Takes its pool
/// explicitly rather than reaching for `db::get()` internally — same
/// testable-all-the-way-down convention every other `db.rs`-touching
/// function in this codebase already follows (see `anthropic::tools`'
/// `pool: &PgPool` parameters), and what lets this store be pointed at a
/// `#[sqlx::test]`-provided throwaway database instead of the real one.
#[derive(Clone)]
pub struct PgCredentialStore {
    pool: PgPool,
    server_id: i64,
    /// The address the manager was built for, and the only one whose
    /// grant this store loads or saves.
    url: String,
    rule: SaveRule,
}

/// What a `PgCredentialStore`'s `save` may write over. `clear` has no
/// rule: removing a grant never sends one anywhere.
#[derive(Clone)]
enum SaveRule {
    /// A login attempt's store (`start`): the new grant is saved while the
    /// row's URL is still the store's, the address the provider issued the
    /// grant for (SME-166).
    SignIn,
    /// A connection's manager, whose only saves are refreshes: saved only
    /// over a grant for the same client (SME-113), at the store's URL.
    RefreshOnly,
}

impl PgCredentialStore {
    /// The store a login attempt for `url` saves its new grant through.
    fn for_sign_in(pool: PgPool, server_id: i64, url: String) -> Self {
        Self { pool, server_id, url, rule: SaveRule::SignIn }
    }

    /// The store a connection's manager for `url` refreshes through
    /// (SME-113).
    fn for_refreshes(pool: PgPool, server_id: i64, url: String) -> Self {
        Self { pool, server_id, url, rule: SaveRule::RefreshOnly }
    }
}

/// Why a sign-in's grant wasn't saved: the server's row no longer has the
/// URL the sign-in started for (SME-166). The callback shows it as it is,
/// not wrapped in rmcp's "Internal error" (review 1).
const ADDRESS_CHANGED_ERROR: &str = "This MCP server's address was changed, or the server deleted, while you were signing in, so the sign-in wasn't saved. Click Connect to sign in to its current address.";

#[async_trait::async_trait]
impl CredentialStore for PgCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let config = db::get_mcp_server_config(&self.pool, self.server_id)
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?
            .ok_or_else(|| {
                AuthError::InternalError(format!("no MCP server config with id {}", self.server_id))
            })?;
        // A manager built for an address the row has since moved off (a
        // connection that outlived an edit) gets no grant: the row's grant
        // is for its new address (SME-166 review 2). rmcp then reports
        // `AuthorizationRequired`.
        if config.url != self.url {
            return Ok(None);
        }
        match config.oauth_credentials {
            None => Ok(None),
            Some(json) => serde_json::from_value(json.0)
                .map(Some)
                .map_err(|e| AuthError::InternalError(e.to_string())),
        }
    }

    /// A sign-in saves only while the row's URL is the one it started for,
    /// so one that finishes after a URL edit or a delete never puts the old
    /// address's token on the new one; the callback then fails with
    /// `ADDRESS_CHANGED_ERROR` (SME-166). A refresh saves only over a grant
    /// for the same client, at the URL its connection was built for, so one that finishes after a Disconnect, a URL
    /// change or a new Connect doesn't bring the old grant back. It then
    /// fails, so the request it was for doesn't go on with a grant the user
    /// removed or replaced (SME-113). Each check is in the `UPDATE` itself,
    /// so nothing can change between it and the write.
    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let value = serde_json::to_value(&credentials)
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        let (saved, refused) = match &self.rule {
            SaveRule::SignIn => (
                db::save_mcp_server_sign_in(&self.pool, self.server_id, &self.url, value).await,
                ADDRESS_CHANGED_ERROR,
            ),
            SaveRule::RefreshOnly => (
                db::save_refreshed_mcp_server_oauth_credentials(
                    &self.pool,
                    self.server_id,
                    &self.url,
                    &credentials.client_id,
                    value,
                )
                .await,
                "the sign-in was removed or replaced while its token was being refreshed",
            ),
        };
        match saved {
            Ok(true) => Ok(()),
            Ok(false) => Err(AuthError::InternalError(refused.to_string())),
            Err(e) => Err(AuthError::InternalError(e.to_string())),
        }
    }

    async fn clear(&self) -> Result<(), AuthError> {
        db::set_mcp_server_oauth_credentials(&self.pool, self.server_id, None)
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))
    }
}

/// One in-flight authorization attempt per server id — holds the whole
/// `OAuthState::Session` and its state store (the PKCE verifier, CSRF token
/// and expected issuer for this attempt) between `start` and the callback,
/// which ends it whatever the outcome. An attempt is bound to the URL it
/// started for: its manager's store saves the grant only while the row
/// still has that URL (SME-166), so an edit or a delete needn't touch it. In-memory only: a
/// restart mid-login just means starting over, the same class of accepted
/// limitation as `mcp.rs`'s connection registry — see SME-16's "Key
/// discovery."
static PENDING: LazyLock<AsyncMutex<HashMap<i64, PendingAttempt>>> =
    LazyLock::new(|| AsyncMutex::new(HashMap::new()));

struct PendingAttempt {
    state: OAuthState,
    /// The attempt's `rmcp` state store, shared with `state`'s manager
    /// (clones share one map). It holds the issuer the attempt expects,
    /// which `rmcp` checks on a code callback but which an error callback
    /// has to be checked against here (`verify_error_callback`).
    stored: InMemoryStateStore,
}

/// How long a failed callback's error waits for its page to take it.
const CALLBACK_ERROR_TTL: Duration = Duration::from_secs(5 * 60);

/// The last failed callback's error per server id, and when it was kept
/// (SME-168). The callback redirects with only `?oauth_callback=1`, and the
/// edit page takes the text from here (`take_callback_error`), so no text
/// in a link can show as smelt's own error. A successful callback clears
/// the slot; a take empties it; an entry older than `CALLBACK_ERROR_TTL`
/// is refused on take and pruned on the next record. An error from a
/// callback that matched no attempt (`record_unmatched_callback_error`)
/// never replaces an unexpired one from a callback that did, so a forged
/// callback can't swap the real error out before the page takes it
/// (SME-177). In memory: a restart
/// between the callback and its page load loses the text, and the status
/// section still says the server isn't connected. A plain `Mutex`: nothing
/// is awaited while it's held.
static CALLBACK_ERRORS: LazyLock<StdMutex<HashMap<i64, KeptCallbackError>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

struct KeptCallbackError {
    message: String,
    kept: Instant,
    /// Whether the callback carried a pending attempt's state.
    matched: bool,
}

fn callback_errors() -> std::sync::MutexGuard<'static, HashMap<i64, KeptCallbackError>> {
    // No critical section leaves the map half-changed.
    CALLBACK_ERRORS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps `message`, from a callback that matched an attempt, as server
/// `id`'s callback error, replacing any earlier one, after pruning the
/// expired entries.
fn record_callback_error(id: i64, message: String) {
    record_callback_error_at(id, message, Instant::now());
}

fn record_callback_error_at(id: i64, message: String, at: Instant) {
    record(id, KeptCallbackError { message, kept: at, matched: true });
}

/// Keeps `message`, from a callback that matched no attempt, unless the
/// slot holds an unexpired error from one that did.
fn record_unmatched_callback_error(id: i64, message: String) {
    record(id, KeptCallbackError { message, kept: Instant::now(), matched: false });
}

fn record(id: i64, error: KeptCallbackError) {
    let mut errors = callback_errors();
    errors.retain(|_, kept| kept.kept.elapsed() < CALLBACK_ERROR_TTL);
    if !error.matched && errors.get(&id).is_some_and(|kept| kept.matched) {
        return;
    }
    errors.insert(id, error);
}

fn clear_callback_error(id: i64) {
    callback_errors().remove(&id);
}

/// Server `id`'s kept callback error, removed so it shows once; `None` if
/// there's none or it's older than `CALLBACK_ERROR_TTL`.
pub fn take_callback_error(id: i64) -> Option<String> {
    let error = callback_errors().remove(&id)?;
    (error.kept.elapsed() < CALLBACK_ERROR_TTL).then_some(error.message)
}

/// Starts an OAuth authorization attempt for `config` and returns the
/// authorization URL the browser must be navigated to. Only one attempt
/// per server is tracked at a time — starting a new one for the same
/// server replaces whatever attempt was pending.
pub async fn start(
    pool: &PgPool,
    config: &McpServerConfig,
    redirect_uri: String,
) -> Result<String, String> {
    crate::mcp::install_crypto_provider();
    let mut manager = AuthorizationManager::new(config.url.as_str())
        .await
        .map_err(|e| {
            format!(
                "failed to initialize OAuth for MCP server {:?}: {e}",
                config.name
            )
        })?;
    manager.set_credential_store(PgCredentialStore::for_sign_in(
        pool.clone(),
        config.id,
        config.url.clone(),
    ));
    let stored = InMemoryStateStore::new();
    manager.set_state_store(stored.clone());

    let mut state = OAuthState::Unauthorized(manager);
    let mut request = AuthorizationRequest::new(redirect_uri).with_client_name("smelt");
    // A provider with no discovery metadata and no Dynamic Client
    // Registration support (GitHub, confirmed live — see SME-16's "Live
    // verification") needs a client pre-registered by hand instead —
    // `AuthorizationRequest`'s own priority order (see
    // `OAuthState::start_authorization`'s doc comment) already prefers
    // this over DCR whenever it's present.
    if let Some(client_id) = config.oauth_client_id.as_deref() {
        request = request.with_preregistered_client(client_id);
        if let Some(client_secret) = config.oauth_client_secret.as_deref() {
            request = request.with_client_secret(client_secret);
        }
    }
    state.start_authorization(request).await.map_err(|e| {
        format!(
            "failed to start OAuth authorization for MCP server {:?}: {e}",
            config.name
        )
    })?;
    let url = state.get_authorization_url().await.map_err(|e| {
        format!(
            "failed to get authorization URL for MCP server {:?}: {e}",
            config.name
        )
    })?;
    // B4 (SME-122): the URL is built from the authorization server's
    // metadata, which rmcp parses but doesn't limit to http(s); the page
    // navigates to it, so a `javascript:` endpoint would run in smelt's
    // origin.
    if !crate::api::mcp::is_navigable_authorization_url(&url) {
        return Err(format!(
            "MCP server {:?}'s authorization server gave a sign-in address that isn't an http or https URL, so smelt won't open it",
            config.name
        ));
    }

    PENDING
        .lock()
        .await
        .insert(config.id, PendingAttempt { state, stored });
    Ok(url)
}

/// The `AuthorizationManager` a server's connections get their tokens
/// from (SME-113): built from the stored grant, so its client is the one
/// that grant was issued to. rmcp's `AuthClient` asks it for a token
/// before every request, and it refreshes one that's expiring, saving the
/// result through `PgCredentialStore`.
pub async fn connection_manager(
    pool: &PgPool,
    config: &McpServerConfig,
) -> Result<AuthorizationManager, String> {
    if config.oauth_credentials.is_none() {
        return Err(format!(
            "MCP server {:?} is configured for OAuth but has never connected — use the Connect button on its edit page",
            config.name
        ));
    }
    crate::mcp::install_crypto_provider();
    let http_client = ProviderHttpClient::new().map_err(|e| {
        format!(
            "failed to initialize OAuth for MCP server {:?}: {e}",
            config.name
        )
    })?;
    let mut manager =
        AuthorizationManager::new_with_oauth_http_client(config.url.as_str(), Arc::new(http_client))
            .await
            .map_err(|e| {
                format!(
                    "failed to initialize OAuth for MCP server {:?}: {e}",
                    config.name
                )
            })?;
    manager.set_credential_store(PgCredentialStore::for_refreshes(
        pool.clone(),
        config.id,
        config.url.clone(),
    ));
    let restored = manager.initialize_from_store().await.map_err(|e| {
        format!(
            "failed to load stored OAuth credentials for MCP server {:?}: {e}",
            config.name
        )
    })?;
    if !restored {
        // No token stored, or rmcp discarded one bound to a provider whose
        // issuer has since changed.
        return Err(format!(
            "MCP server {:?} has no usable sign-in — use the Connect button on its edit page",
            config.name
        ));
    }
    // rmcp configures the client from the stored client id alone, so a
    // pre-registered client's refresh went out without its secret, and
    // GitHub refuses that (`invalid_client`). The secret goes back on the
    // same way `start` gives it to the code exchange, for the grant that
    // client was issued.
    let stored_client_id = config
        .oauth_credentials
        .as_ref()
        .and_then(|json| json.0.get("client_id"))
        .and_then(|id| id.as_str());
    if let (Some(client_id), Some(secret)) = (
        config.oauth_client_id.as_deref(),
        config.oauth_client_secret.as_deref(),
    ) && stored_client_id == Some(client_id)
    {
        let mut client = OAuthClientConfig::new(client_id, config.url.as_str())
            .with_client_secret(secret);
        // Keep the manager's own application type, as rmcp does when it
        // configures a stored client.
        client.application_type = None;
        manager.configure_client(client).map_err(|e| {
            format!(
                "failed to configure the OAuth client for MCP server {:?}: {e}",
                config.name
            )
        })?;
    }
    Ok(manager)
}

/// The HTTP client a connection's `AuthorizationManager` talks to its
/// provider through: what rmcp's own default does (a 30 s timeout, the
/// redirect policy each request asks for, a 1 MiB cap on a reply), plus
/// one repair (SME-113 review 1). GitHub answers a refused token request
/// with HTTP 200 and an `error` in the body, and names a dead refresh token
/// `bad_refresh_token`, where RFC 6749 §5.2 has a 400 and `invalid_grant`.
/// oauth2 then can't parse a token from the 200 and rmcp reports a failed
/// request (`TokenRefreshFailed`), so a sign-in that needs a Reconnect read
/// as Unreachable. A POST's 200 carrying an `error` and no `access_token` is
/// passed on as the 400 it means, with `bad_refresh_token` as
/// `invalid_grant`, which rmcp turns into `AuthorizationRequired`.
struct ProviderHttpClient {
    follow_redirects: reqwest_rmcp::Client,
    stop_redirects: reqwest_rmcp::Client,
}

const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_PROVIDER_REPLY_BYTES: usize = 1024 * 1024;

impl ProviderHttpClient {
    fn new() -> Result<Self, reqwest_rmcp::Error> {
        Ok(Self {
            follow_redirects: reqwest_rmcp::Client::builder()
                .timeout(PROVIDER_TIMEOUT)
                .build()?,
            stop_redirects: reqwest_rmcp::Client::builder()
                .timeout(PROVIDER_TIMEOUT)
                .redirect(reqwest_rmcp::redirect::Policy::none())
                .build()?,
        })
    }
}

/// RFC 6749 §5.2's shape for a token request GitHub refused with a 200:
/// `Some(body)` to pass on as a 400, `None` to leave the reply alone.
fn as_token_error(status: http::StatusCode, body: &[u8]) -> Option<Vec<u8>> {
    if status != http::StatusCode::OK {
        return None;
    }
    let mut reply: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(body).ok()?;
    if reply.contains_key("access_token") || !reply.get("error")?.is_string() {
        return None;
    }
    if reply.get("error").and_then(|e| e.as_str()) == Some("bad_refresh_token") {
        reply.insert("error".to_string(), "invalid_grant".into());
    }
    serde_json::to_vec(&reply).ok()
}

impl rmcp::transport::auth::OAuthHttpClient for ProviderHttpClient {
    fn execute(
        &self,
        request: rmcp::transport::auth::OAuthHttpRequest,
    ) -> rmcp::transport::auth::OAuthHttpClientFuture<'_> {
        use futures_util::StreamExt;
        use rmcp::transport::auth::{OAuthHttpClientError, OAuthHttpRedirectPolicy};
        Box::pin(async move {
            let client = match request.redirect_policy {
                OAuthHttpRedirectPolicy::Stop => &self.stop_redirects,
                _ => &self.follow_redirects,
            };
            let is_post = request.request.method() == http::Method::POST;
            let request = reqwest_rmcp::Request::try_from(request.request)
                .map_err(|e| Box::new(e) as OAuthHttpClientError)?;
            let response = client
                .execute(request)
                .await
                .map_err(|e| Box::new(e) as OAuthHttpClientError)?;
            let status = response.status();
            let mut builder = http::Response::builder()
                .status(status)
                .version(response.version());
            // The body is passed on whole, and may be replaced below, so
            // its length isn't copied.
            for (name, value) in response.headers() {
                if name != http::header::CONTENT_LENGTH {
                    builder = builder.header(name, value);
                }
            }
            let mut body = Vec::new();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(|e| Box::new(e) as OAuthHttpClientError)?;
                if chunk.len() > MAX_PROVIDER_REPLY_BYTES - body.len() {
                    return Err(format!(
                        "the OAuth provider's reply is over {MAX_PROVIDER_REPLY_BYTES} bytes"
                    )
                    .into());
                }
                body.extend_from_slice(&chunk);
            }
            if is_post && let Some(error) = as_token_error(status, &body) {
                builder = builder.status(http::StatusCode::BAD_REQUEST);
                body = error;
            }
            builder.body(body).map_err(|e| Box::new(e) as OAuthHttpClientError)
        })
    }
}

/// Completes an in-flight authorization attempt — pops `server_id`'s
/// pending `OAuthState`, exchanges `code` for tokens, and persists them via
/// `PgCredentialStore` (through `OAuthState::handle_callback_with_issuer`'s
/// own call into the manager's configured credential store).
/// `code`/`csrf_token`/`issuer` come straight from the callback request's
/// own `code`/`state`/`iss` query params (see `callback_handler` below) —
/// using the lower-level `handle_callback_with_issuer` rather than
/// `handle_callback_url` avoids needing to reconstruct an absolute URL from
/// the incoming Axum request just to have `rmcp` immediately re-parse it
/// back into the same values. `rmcp` checks `issuer` against the one the
/// provider's metadata advertised (RFC 9207, MCP's SEP-2468), and refuses a
/// callback without one from a provider that says it sends it — GitHub
/// and Linear both do (SME-65).
///
/// Only a callback carrying the attempt's `state` ends it
/// (`take_matching_attempt`, SME-177); any other is refused with the
/// attempt left pending.
async fn handle_callback(
    server_id: i64,
    code: &str,
    csrf_token: &str,
    issuer: Option<&str>,
) -> Result<(), CallbackFailure> {
    let mut attempt = take_matching_attempt(server_id, Some(csrf_token))
        .await
        .map_err(|unmatched| {
            CallbackFailure::unmatched(match unmatched {
                Unmatched::NoAttempt => NO_ATTEMPT_ERROR,
                Unmatched::NotItsState => UNVERIFIED_ERROR,
            })
        })?;
    attempt
        .state
        .handle_callback_with_issuer(code, csrf_token, issuer)
        .await
        .map_err(|e| {
            CallbackFailure::matched(match e {
                AuthError::InternalError(reason) if reason == ADDRESS_CHANGED_ERROR => reason,
                e => format!("OAuth callback failed: {e}"),
            })
        })
}

const NO_ATTEMPT_ERROR: &str = "no OAuth authorization attempt in progress for this server — it may have expired, or this callback link was already used";

/// Why a callback ended no attempt.
enum Unmatched {
    /// Nothing is pending for the server.
    NoAttempt,
    /// An attempt is pending, but the callback's `state` isn't its own (a
    /// forged or replayed callback, or one for an attempt a second Connect
    /// replaced), or the callback has none.
    NotItsState,
}

/// Removes and returns `server_id`'s pending attempt only if `csrf_token`
/// is its state, as recorded in the attempt's own state store (the one
/// `rmcp` checks). The check and the removal happen under one `PENDING`
/// guard, so a forged callback and the genuine one arriving together can't
/// interleave between them: anyone can send a request to the callback
/// route, so one without the state leaves the user's sign-in pending
/// (SME-177 B23).
async fn take_matching_attempt(
    server_id: i64,
    csrf_token: Option<&str>,
) -> Result<PendingAttempt, Unmatched> {
    let mut pending = PENDING.lock().await;
    let attempt = pending.get(&server_id).ok_or(Unmatched::NoAttempt)?;
    let csrf_token = csrf_token.ok_or(Unmatched::NotItsState)?;
    // `InMemoryStateStore::load` only reads, and can't fail.
    if attempt.stored.load(csrf_token).await.ok().flatten().is_none() {
        return Err(Unmatched::NotItsState);
    }
    pending.remove(&server_id).ok_or(Unmatched::NoAttempt)
}

/// A failed callback: the text the edit page shows, and whether the
/// callback carried a pending attempt's state (and so ended it).
#[derive(Debug)]
struct CallbackFailure {
    message: String,
    matched: bool,
}

impl CallbackFailure {
    fn matched(message: impl Into<String>) -> Self {
        Self { message: message.into(), matched: true }
    }

    fn unmatched(message: impl Into<String>) -> Self {
        Self { message: message.into(), matched: false }
    }
}

impl std::fmt::Display for CallbackFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Clears `config`'s stored OAuth credentials without touching anything
/// else about the row — the edit page's "Disconnect" button.
pub async fn disconnect(pool: &PgPool, config: &McpServerConfig) -> Result<(), String> {
    db::set_mcp_server_oauth_credentials(pool, config.id, None)
        .await
        .map_err(|e| format!("failed to disconnect MCP server {:?}: {e}", config.name))
}

/// `SMELT_BASE_URL`, if set to a non-blank value: smelt's own address. For
/// OAuth it overrides the `Host`/`X-Forwarded-Proto`-derived guess below
/// when that's wrong (e.g. smelt reachable through a tunnel or proxy that
/// doesn't forward a usable `Host`); a preview page links back to smelt
/// with it (SME-46). Set-but-empty is treated as unset, same rule every other env
/// var in this codebase follows. Trims a trailing
/// slash so callers can always append `/oauth/mcp-callback/{id}` directly.
pub(crate) fn smelt_base_url() -> Option<String> {
    let value = std::env::var("SMELT_BASE_URL").ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.trim_end_matches('/').to_string())
}

/// The scheme+host to build a redirect_uri from. `SMELT_BASE_URL` wins if
/// set; otherwise derived from the current request — see SME-16's
/// "Answered by the user." A bare `Host` header never carries scheme, so
/// `X-Forwarded-Proto` (set by the reverse proxy in front of smelt in every
/// deployment this matters for) decides `https` vs. `http`, defaulting to
/// `http` when absent (e.g. plain local dev).
pub async fn request_base_url() -> Result<String, dioxus::prelude::ServerFnError> {
    if let Some(base_url) = smelt_base_url() {
        return Ok(base_url);
    }
    let headers =
        dioxus::prelude::dioxus_fullstack::FullstackContext::extract::<axum::http::HeaderMap, _>()
            .await?;
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    Ok(format!("{scheme}://{host}"))
}

#[derive(serde::Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    /// The provider's issuer (RFC 9207), sent by a provider whose metadata
    /// sets `authorization_response_iss_parameter_supported`.
    iss: Option<String>,
    /// Set instead of `code`/`state` when the user denies consent or the
    /// provider itself fails (RFC 6749 §4.1.2.1) — surfaced like any other
    /// failure rather than treated as a missing-param bug, once
    /// `verify_error_callback` has tied it to the pending attempt.
    error: Option<String>,
    error_description: Option<String>,
}

/// A plain Axum route (`main.rs`'s `build_router()`), not a Dioxus server
/// function — see SME-16's "API shape" for why:
/// this is hit by the user's browser because the *authorization provider*
/// redirected it, and it must respond with a real HTTP redirect back into
/// the app, not JSON. `id` is embedded directly in the path (the
/// redirect_uri `start` registers), so the handler always knows which
/// server's flow this is without a further lookup.
pub async fn callback_handler(
    axum::extract::Path(id): axum::extract::Path<i64>,
    axum::extract::Query(params): axum::extract::Query<CallbackParams>,
) -> axum::response::Redirect {
    // No pool parameter needed here: the `OAuthState` popped from `PENDING`
    // already carries the `PgCredentialStore` `start` configured it with,
    // so `handle_callback` alone persists the exchanged tokens.
    // The error stays on the server and the redirect carries only a flag
    // (SME-168): a text in the URL would let any link put its own words in
    // smelt's error styling.
    let outcome = complete_callback(id, params).await;
    match outcome {
        Ok(()) => {
            clear_callback_error(id);
            axum::response::Redirect::to(&format!("/mcp-servers/{id}"))
        }
        Err(CallbackFailure { message, matched: true }) => {
            record_callback_error(id, message);
            axum::response::Redirect::to(&format!("/mcp-servers/{id}?oauth_callback=1"))
        }
        Err(CallbackFailure { message, matched: false }) => {
            record_unmatched_callback_error(id, message);
            axum::response::Redirect::to(&format!("/mcp-servers/{id}?oauth_callback=1"))
        }
    }
}

/// Shown in place of the provider's own text when an error callback can't
/// be tied to this server's pending attempt and to the issuer that attempt
/// expects (RFC 9207 §2.4): anyone can send an error callback, so its text
/// is only worth showing once it's checked.
const UNVERIFIED_ERROR: &str = "smelt couldn't verify the authorization provider's response, so it was ignored. Click Connect to try again.";

async fn complete_callback(id: i64, params: CallbackParams) -> Result<(), CallbackFailure> {
    if let Some(error) = params.error {
        verify_error_callback(id, params.state.as_deref(), params.iss.as_deref()).await?;
        return Err(CallbackFailure::matched(
            params.error_description.unwrap_or(error),
        ));
    }
    // A missing parameter returns before `PENDING` is touched.
    let code = params.code.ok_or_else(|| {
        CallbackFailure::unmatched("authorization callback is missing its code parameter")
    })?;
    let state = params.state.ok_or_else(|| {
        CallbackFailure::unmatched("authorization callback is missing its state parameter")
    })?;
    handle_callback(id, &code, &state, params.iss.as_deref()).await?;
    crate::mcp::evict(id).await;
    Ok(())
}

/// Checks an error callback belongs to `server_id`'s pending attempt: its
/// `state` must be the attempt's, and its `iss` must pass the check `rmcp`
/// gives a code callback (RFC 9207 §2.4). `Err(UNVERIFIED_ERROR)` if not.
/// A callback with the attempt's state ends it (a provider doesn't send a
/// code after an error), whatever its `iss`: only the provider's redirect
/// can carry the state. One without it leaves the attempt pending
/// (SME-177 B23).
async fn verify_error_callback(
    server_id: i64,
    csrf_token: Option<&str>,
    issuer: Option<&str>,
) -> Result<(), CallbackFailure> {
    let attempt = take_matching_attempt(server_id, csrf_token)
        .await
        .map_err(|_| CallbackFailure::unmatched(UNVERIFIED_ERROR))?;
    let stored = match csrf_token {
        Some(csrf_token) => attempt.stored.load(csrf_token).await.ok().flatten(),
        None => None,
    };
    match stored {
        Some(stored) if issuer_accepted(&stored, issuer) => Ok(()),
        _ => Err(CallbackFailure::matched(UNVERIFIED_ERROR)),
    }
}

/// `rmcp`'s own rule for a code callback's `iss`
/// (`AuthorizationManager::validate_authorization_response_issuer`, private
/// in rmcp 3.1.2): equal to the recorded issuer if one was sent, present if
/// the provider advertised sending it, and absent if no issuer was recorded.
fn issuer_accepted(stored: &StoredAuthorizationState, received: Option<&str>) -> bool {
    match (stored.expected_issuer.as_deref(), received) {
        (Some(expected), Some(received)) => received == expected,
        (Some(_), None) => !stored.require_issuer,
        (None, received) => received.is_none() && !stored.require_issuer,
    }
}

#[cfg(test)]
mod tests;
