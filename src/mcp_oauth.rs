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
use std::sync::LazyLock;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, InMemoryStateStore,
    OAuthState, StateStore, StoredAuthorizationState, StoredCredentials,
};
use sqlx::PgPool;
use tokio::sync::Mutex as AsyncMutex;

use crate::db::{self, McpServerConfig};

/// Postgres-backed `CredentialStore` for one server's OAuth grant, bound to
/// a single `mcp_servers.id`. Plugged into each `AuthorizationManager`
/// smelt builds (`start`, and `connection_manager` for connections); it
/// keeps nothing in memory, so every load reads the row fresh. Takes its pool
/// explicitly rather than reaching for `db::get()` internally — same
/// testable-all-the-way-down convention every other `db.rs`-touching
/// function in this codebase already follows (see `anthropic::tools`'
/// `pool: &PgPool` parameters), and what lets this store be pointed at a
/// `#[sqlx::test]`-provided throwaway database instead of the real one.
#[derive(Clone)]
pub struct PgCredentialStore {
    pool: PgPool,
    server_id: i64,
}

impl PgCredentialStore {
    pub fn new(pool: PgPool, server_id: i64) -> Self {
        Self { pool, server_id }
    }
}

#[async_trait::async_trait]
impl CredentialStore for PgCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let config = db::get_mcp_server_config(&self.pool, self.server_id)
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?
            .ok_or_else(|| {
                AuthError::InternalError(format!("no MCP server config with id {}", self.server_id))
            })?;
        match config.oauth_credentials {
            None => Ok(None),
            Some(json) => serde_json::from_value(json.0)
                .map(Some)
                .map_err(|e| AuthError::InternalError(e.to_string())),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let value = serde_json::to_value(&credentials)
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        db::set_mcp_server_oauth_credentials(&self.pool, self.server_id, Some(value))
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))
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
/// which ends it whatever the outcome. In-memory only: a
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
    manager.set_credential_store(PgCredentialStore::new(pool.clone(), config.id));
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
    let mut manager = AuthorizationManager::new(config.url.as_str())
        .await
        .map_err(|e| {
            format!(
                "failed to initialize OAuth for MCP server {:?}: {e}",
                config.name
            )
        })?;
    manager.set_credential_store(PgCredentialStore::new(pool.clone(), config.id));
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
    Ok(manager)
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
pub async fn handle_callback(
    server_id: i64,
    code: &str,
    csrf_token: &str,
    issuer: Option<&str>,
) -> Result<(), String> {
    let mut attempt = PENDING.lock().await.remove(&server_id).ok_or_else(|| {
        "no OAuth authorization attempt in progress for this server — it may have expired, or this callback link was already used".to_string()
    })?;
    attempt
        .state
        .handle_callback_with_issuer(code, csrf_token, issuer)
        .await
        .map_err(|e| format!("OAuth callback failed: {e}"))
}

/// Clears `config`'s stored OAuth credentials without touching anything
/// else about the row — the edit page's "Disconnect" button.
pub async fn disconnect(pool: &PgPool, config: &McpServerConfig) -> Result<(), String> {
    PgCredentialStore::new(pool.clone(), config.id)
        .clear()
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
    let outcome = complete_callback(id, params).await;
    match outcome {
        Ok(()) => axum::response::Redirect::to(&format!("/mcp-servers/{id}")),
        Err(error) => {
            let encoded: String =
                percent_encoding::utf8_percent_encode(&error, percent_encoding::NON_ALPHANUMERIC)
                    .collect();
            axum::response::Redirect::to(&format!("/mcp-servers/{id}?oauth_error={encoded}"))
        }
    }
}

/// Shown in place of the provider's own text when an error callback can't
/// be tied to this server's pending attempt and to the issuer that attempt
/// expects (RFC 9207 §2.4): anyone can send an error callback, so its text
/// is only worth showing once it's checked.
const UNVERIFIED_ERROR: &str = "smelt couldn't verify the authorization provider's response, so it was ignored. Click Connect to try again.";

async fn complete_callback(id: i64, params: CallbackParams) -> Result<(), String> {
    if let Some(error) = params.error {
        verify_error_callback(id, params.state.as_deref(), params.iss.as_deref()).await?;
        return Err(params.error_description.unwrap_or(error));
    }
    let code = params
        .code
        .ok_or_else(|| "authorization callback is missing its code parameter".to_string())?;
    let state = params
        .state
        .ok_or_else(|| "authorization callback is missing its state parameter".to_string())?;
    handle_callback(id, &code, &state, params.iss.as_deref()).await?;
    crate::mcp::evict(id).await;
    Ok(())
}

/// Ends `server_id`'s pending attempt (a provider doesn't send a code
/// after an error), and checks the error callback belongs to it: its
/// `state` must be the attempt's, and its `iss` must pass the check `rmcp`
/// gives a code callback (RFC 9207 §2.4). `Err(UNVERIFIED_ERROR)` if not.
async fn verify_error_callback(
    server_id: i64,
    csrf_token: Option<&str>,
    issuer: Option<&str>,
) -> Result<(), String> {
    let unverified = || UNVERIFIED_ERROR.to_string();
    let attempt = PENDING
        .lock()
        .await
        .remove(&server_id)
        .ok_or_else(unverified)?;
    let stored = attempt
        .stored
        .load(csrf_token.ok_or_else(unverified)?)
        .await
        .ok()
        .flatten()
        .ok_or_else(unverified)?;
    if issuer_accepted(&stored, issuer) {
        Ok(())
    } else {
        Err(unverified())
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
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicI64, Ordering};

    use axum::Json;
    use axum::body::Bytes;
    use axum::routing::{get, post};

    /// `SMELT_BASE_URL` is process-global and only this test touches it —
    /// no cross-test lock needed. Save/restore around the body
    /// so a run order that puts another test after this one never sees a
    /// value this test set.
    #[test]
    fn test_smelt_base_url_treats_unset_and_blank_as_none_and_trims_a_trailing_slash() {
        let original = std::env::var("SMELT_BASE_URL").ok();

        unsafe { std::env::remove_var("SMELT_BASE_URL") };
        assert_eq!(smelt_base_url(), None);

        unsafe { std::env::set_var("SMELT_BASE_URL", "") };
        assert_eq!(
            smelt_base_url(),
            None,
            "set-but-empty should be treated as unset, same as every other env var here"
        );

        unsafe { std::env::set_var("SMELT_BASE_URL", "https://smelt.example.com") };
        assert_eq!(
            smelt_base_url(),
            Some("https://smelt.example.com".to_string())
        );

        unsafe { std::env::set_var("SMELT_BASE_URL", "https://smelt.example.com/") };
        assert_eq!(
            smelt_base_url(),
            Some("https://smelt.example.com".to_string()),
            "a trailing slash must be stripped so callers can always append /oauth/mcp-callback/{{id}} directly"
        );

        match original {
            Some(value) => unsafe { std::env::set_var("SMELT_BASE_URL", value) },
            None => unsafe { std::env::remove_var("SMELT_BASE_URL") },
        }
    }

    /// A minimal OAuth authorization server: dynamic client registration
    /// (`/register`) and token exchange/refresh (`/token`), plus an RFC
    /// 8414 metadata document only when a test asks for one
    /// (`MockMetadata`). Without it, `AuthorizationManager` treats the
    /// server as having given no evidence of OAuth support and falls back
    /// to the MCP spec's legacy default endpoints (`/authorize`, `/token`,
    /// `/register` under the base URL), which is exactly what this mock
    /// implements, so nothing further to fake. With it, `rmcp` records the
    /// advertised issuer, which is what makes its RFC 9207 checks run.
    /// `/authorize` itself is never actually hit — a real flow needs a
    /// live browser to visit it, which this test doesn't have; getting an
    /// authorization *URL* back from `start` is enough to prove that step
    /// worked, and the callback is simulated directly (see the tests below).
    async fn register_handler() -> Json<serde_json::Value> {
        Json(serde_json::json!({ "client_id": "test-client", "redirect_uris": [] }))
    }

    /// The initial code exchange hands back a token that's already
    /// expired (`expires_in: 0`) — deterministic proof that a later
    /// `get_access_token()` call refreshes rather than reusing it, with no
    /// sleep needed. A `grant_type=refresh_token` request gets a
    /// distinctly-named token back so the test can tell which one it got.
    async fn token_handler(body: Bytes) -> Json<serde_json::Value> {
        let body = String::from_utf8_lossy(&body);
        let is_refresh = body.contains("grant_type=refresh_token");
        Json(serde_json::json!({
            "access_token": if is_refresh { "refreshed-access-token" } else { "initial-access-token" },
            "token_type": "bearer",
            "expires_in": if is_refresh { 3600 } else { 0 },
            "refresh_token": "test-refresh-token",
        }))
    }

    /// Which RFC 8414 metadata document the mock serves at
    /// `/.well-known/oauth-authorization-server`, if any.
    #[derive(Clone, Copy)]
    enum MockMetadata {
        /// No document: rmcp falls back to the legacy default endpoints and
        /// records no issuer, as it did for GitHub before it published one.
        None,
        /// `issuer` is the mock's own URL, as RFC 8414 §3.3 requires.
        Issuer { iss_supported: bool },
        /// `issuer` names a different server from the one serving it.
        WrongIssuer,
    }

    /// The issuer the mock at `base_url` advertises — what a callback's
    /// `iss` must equal.
    fn mock_issuer(base_url: &str) -> String {
        base_url.trim_end_matches('/').to_string()
    }

    async fn spawn_mock_oauth_server(metadata: MockMetadata) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock oauth server");
        let addr = listener.local_addr().expect("mock oauth server local addr");
        let base_url = format!("http://{addr}/");
        let mut app = axum::Router::new()
            .route("/register", post(register_handler))
            .route("/token", post(token_handler));
        let (issuer, iss_supported) = match metadata {
            MockMetadata::None => (None, false),
            MockMetadata::Issuer { iss_supported } => (Some(mock_issuer(&base_url)), iss_supported),
            MockMetadata::WrongIssuer => (Some("http://127.0.0.1:1".to_string()), true),
        };
        if let Some(issuer) = issuer {
            let document = serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{base_url}authorize"),
                "token_endpoint": format!("{base_url}token"),
                "registration_endpoint": format!("{base_url}register"),
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": iss_supported,
            });
            app = app.route(
                "/.well-known/oauth-authorization-server",
                get(move || async move { Json(document) }),
            );
        }
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock oauth server");
        });
        base_url
    }

    /// Server ids key `PENDING`, which every test in the process shares,
    /// while each `#[sqlx::test]` database numbers its rows from 1. Giving
    /// each test's server an id of its own keeps parallel tests' login
    /// attempts apart.
    async fn create_oauth_server(pool: &PgPool, base_url: &str) -> McpServerConfig {
        static NEXT_ID: AtomicI64 = AtomicI64::new(10_000_000);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        sqlx::query(&format!(
            "ALTER TABLE mcp_servers ALTER COLUMN id RESTART WITH {id}"
        ))
        .execute(pool)
        .await
        .expect("set the next mcp_servers id");
        db::create_mcp_server_config(
            pool,
            &format!("oauth-test-server-{id}"),
            base_url,
            &HashMap::new(),
            "oauth",
            None,
            None,
        )
        .await
        .expect("create mcp server config")
    }

    /// Starts a login attempt for `config` and returns its `state`, as the
    /// provider would send it back on the callback.
    async fn start_attempt(pool: &PgPool, config: &McpServerConfig) -> String {
        let authorization_url = start(
            pool,
            config,
            "http://localhost/oauth/mcp-callback/1".to_string(),
        )
        .await
        .expect("start should succeed");
        extract_query_param(&authorization_url, "state")
            .expect("authorization url should carry a state param")
    }

    fn code_callback(state: &str, iss: Option<&str>) -> CallbackParams {
        CallbackParams {
            code: Some("test-code".to_string()),
            state: Some(state.to_string()),
            iss: iss.map(str::to_string),
            error: None,
            error_description: None,
        }
    }

    async fn stored_credentials(pool: &PgPool, server_id: i64) -> Option<serde_json::Value> {
        db::get_mcp_server_config(pool, server_id)
            .await
            .expect("get mcp server config")
            .expect("row should exist")
            .oauth_credentials
            .map(|json| json.0)
    }

    #[sqlx::test]
    async fn test_a_callback_carrying_the_advertised_issuer_saves_credentials(pool: PgPool) {
        let base_url = spawn_mock_oauth_server(MockMetadata::Issuer {
            iss_supported: true,
        })
        .await;
        let config = create_oauth_server(&pool, &base_url).await;
        let state = start_attempt(&pool, &config).await;

        complete_callback(
            config.id,
            code_callback(&state, Some(&mock_issuer(&base_url))),
        )
        .await
        .expect("a callback carrying the advertised issuer should succeed");

        assert!(
            stored_credentials(&pool, config.id).await.is_some(),
            "credentials should be stored after a successful callback"
        );
    }

    /// Pulls `state`'s value out of the authorization URL `start` returns
    /// — standing in for the real browser round-trip a live flow would
    /// need (the provider's `/authorize` page reflecting it back on
    /// redirect). Percent-decodes with the same crate already used for
    /// `callback_handler`'s error encoding, rather than adding a URL-
    /// parsing dependency just for this.
    fn extract_query_param(url: &str, key: &str) -> Option<String> {
        let query = url.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| {
                percent_encoding::percent_decode_str(v)
                    .decode_utf8_lossy()
                    .into_owned()
            })
        })
    }

    /// A rejected callback ends its attempt: retrying the same callback,
    /// even a valid one, finds nothing pending.
    async fn assert_attempt_cleared(server_id: i64, state: &str, base_url: &str) {
        let retry = complete_callback(
            server_id,
            code_callback(state, Some(&mock_issuer(base_url))),
        )
        .await;
        let error = retry.expect_err("the attempt should be gone after a rejected callback");
        assert!(
            error.contains("no OAuth authorization attempt in progress"),
            "unexpected error: {error}"
        );
    }

    #[sqlx::test]
    async fn test_a_callback_without_the_advertised_issuer_is_rejected(pool: PgPool) {
        let base_url = spawn_mock_oauth_server(MockMetadata::Issuer {
            iss_supported: true,
        })
        .await;
        let config = create_oauth_server(&pool, &base_url).await;
        let state = start_attempt(&pool, &config).await;

        let error = complete_callback(config.id, code_callback(&state, None))
            .await
            .expect_err("a provider that advertises iss must send it");

        assert!(
            error.contains("missing required issuer"),
            "unexpected error: {error}"
        );
        assert!(stored_credentials(&pool, config.id).await.is_none());
        assert_attempt_cleared(config.id, &state, &base_url).await;
    }

    #[sqlx::test]
    async fn test_a_callback_from_a_different_issuer_is_rejected(pool: PgPool) {
        let base_url = spawn_mock_oauth_server(MockMetadata::Issuer {
            iss_supported: true,
        })
        .await;
        let config = create_oauth_server(&pool, &base_url).await;
        let state = start_attempt(&pool, &config).await;

        let error = complete_callback(
            config.id,
            code_callback(&state, Some("https://attacker.example")),
        )
        .await
        .expect_err("an iss other than the advertised issuer must be refused");

        assert!(
            error.contains("issuer mismatch"),
            "unexpected error: {error}"
        );
        assert!(stored_credentials(&pool, config.id).await.is_none());
        assert_attempt_cleared(config.id, &state, &base_url).await;
    }

    /// SEP-2468 only requires `iss` from a provider that advertises it.
    #[sqlx::test]
    async fn test_a_callback_without_iss_succeeds_when_the_provider_does_not_advertise_it(
        pool: PgPool,
    ) {
        let base_url = spawn_mock_oauth_server(MockMetadata::Issuer {
            iss_supported: false,
        })
        .await;
        let config = create_oauth_server(&pool, &base_url).await;
        let state = start_attempt(&pool, &config).await;

        complete_callback(config.id, code_callback(&state, None))
            .await
            .expect("iss is optional from a provider that doesn't advertise it");

        assert!(stored_credentials(&pool, config.id).await.is_some());
    }

    /// What a provider sends back instead of a code when the user denies
    /// consent (RFC 6749 §4.1.2.1).
    fn error_callback(state: Option<&str>, iss: Option<&str>) -> CallbackParams {
        CallbackParams {
            code: None,
            state: state.map(str::to_string),
            iss: iss.map(str::to_string),
            error: Some("access_denied".to_string()),
            error_description: Some("The user denied access".to_string()),
        }
    }

    #[sqlx::test]
    async fn test_an_error_callback_from_the_advertised_issuer_shows_its_error_and_ends_the_attempt(
        pool: PgPool,
    ) {
        let base_url = spawn_mock_oauth_server(MockMetadata::Issuer {
            iss_supported: true,
        })
        .await;
        let config = create_oauth_server(&pool, &base_url).await;
        let state = start_attempt(&pool, &config).await;

        let error = complete_callback(
            config.id,
            error_callback(Some(&state), Some(&mock_issuer(&base_url))),
        )
        .await
        .expect_err("an error callback is a failed attempt");

        assert_eq!(error, "The user denied access");
        assert_attempt_cleared(config.id, &state, &base_url).await;
    }

    /// An error callback smelt can't tie to the attempt and its issuer shows
    /// `UNVERIFIED_ERROR`, not the provider's text, and still ends the
    /// attempt.
    #[sqlx::test]
    async fn test_an_error_callback_that_cannot_be_verified_hides_its_text(pool: PgPool) {
        let base_url = spawn_mock_oauth_server(MockMetadata::Issuer {
            iss_supported: true,
        })
        .await;
        let config = create_oauth_server(&pool, &base_url).await;
        let issuer = mock_issuer(&base_url);
        let cases: [(&str, Option<&str>, Option<&str>); 4] = [
            ("no iss", Some("STATE"), None),
            (
                "a different iss",
                Some("STATE"),
                Some("https://attacker.example"),
            ),
            ("an unknown state", Some("not-the-state"), Some(&issuer)),
            ("no state", None, Some(&issuer)),
        ];
        for (case, state_param, iss) in cases {
            let state = start_attempt(&pool, &config).await;
            let state_param = state_param.map(|s| if s == "STATE" { state.as_str() } else { s });

            let error = complete_callback(config.id, error_callback(state_param, iss))
                .await
                .expect_err("an error callback is a failed attempt");

            assert_eq!(error, UNVERIFIED_ERROR, "case: {case}");
            assert_attempt_cleared(config.id, &state, &base_url).await;
        }
    }

    #[sqlx::test]
    async fn test_an_error_callback_with_no_pending_attempt_hides_its_text(pool: PgPool) {
        let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
        let config = create_oauth_server(&pool, &base_url).await;

        let error = complete_callback(config.id, error_callback(Some("some-state"), None))
            .await
            .expect_err("an error callback is a failed attempt");

        assert_eq!(error, UNVERIFIED_ERROR);
    }

    /// SEP-2468 only requires `iss` from a provider that advertises it, on
    /// error callbacks as on code callbacks.
    #[sqlx::test]
    async fn test_an_error_callback_without_iss_shows_its_error_when_the_provider_does_not_advertise_it(
        pool: PgPool,
    ) {
        for metadata in [
            MockMetadata::Issuer {
                iss_supported: false,
            },
            MockMetadata::None,
        ] {
            let base_url = spawn_mock_oauth_server(metadata).await;
            let config = create_oauth_server(&pool, &base_url).await;
            let state = start_attempt(&pool, &config).await;

            let error = complete_callback(config.id, error_callback(Some(&state), None))
                .await
                .expect_err("an error callback is a failed attempt");

            assert_eq!(error, "The user denied access");
        }
    }

    /// RFC 8414 §3.3: metadata must name the server it was fetched from.
    /// `rmcp` enforces this; the test checks it still does through `start`.
    #[sqlx::test]
    async fn test_start_refuses_metadata_naming_a_different_issuer(pool: PgPool) {
        let base_url = spawn_mock_oauth_server(MockMetadata::WrongIssuer).await;
        let config = create_oauth_server(&pool, &base_url).await;

        let error = start(
            &pool,
            &config,
            "http://localhost/oauth/mcp-callback/1".to_string(),
        )
        .await
        .expect_err("metadata for another issuer must be refused");

        assert!(
            error.contains("issuer mismatch"),
            "unexpected error: {error}"
        );
    }

    #[sqlx::test]
    async fn test_start_and_handle_callback_persists_credentials_and_a_later_fetch_refreshes(
        pool: PgPool,
    ) {
        let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
        let created = db::create_mcp_server_config(
            &pool,
            "oauth-test-server",
            &base_url,
            &HashMap::new(),
            "oauth",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let authorization_url = start(
            &pool,
            &created,
            "http://localhost/oauth/mcp-callback/1".to_string(),
        )
        .await
        .expect("start should succeed");
        let state_param = extract_query_param(&authorization_url, "state")
            .expect("authorization url should carry a state param");

        handle_callback(created.id, "test-code", &state_param, None)
            .await
            .expect("callback should succeed");

        let stored = db::get_mcp_server_config(&pool, created.id)
            .await
            .expect("get mcp server config")
            .expect("row should exist")
            .oauth_credentials
            .expect("credentials should be stored after a successful callback");
        assert!(
            stored.0.get("token_response").is_some(),
            "stored credentials should include the exchanged token"
        );

        // `get_access_token` on a fresh manager built from the same store
        // mirrors what `connection_manager`'s manager does before each
        // request — proves both persistence and transparent refresh
        // (the initial token's `expires_in: 0` forces this).
        let mut manager = rmcp::transport::auth::AuthorizationManager::new(base_url.as_str())
            .await
            .expect("new manager");
        manager.set_credential_store(PgCredentialStore::new(pool.clone(), created.id));
        manager
            .initialize_from_store()
            .await
            .expect("initialize_from_store");
        let token = manager
            .get_access_token()
            .await
            .expect("get_access_token should refresh transparently");
        assert_eq!(token, "refreshed-access-token");
    }

    #[sqlx::test]
    async fn test_handle_callback_without_a_pending_attempt_is_a_clear_error(pool: PgPool) {
        let _ = pool;
        let result = handle_callback(999_999, "some-code", "some-state", None).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("no OAuth authorization attempt in progress")
        );
    }

    #[sqlx::test]
    async fn test_disconnect_clears_stored_credentials(pool: PgPool) {
        let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
        let created = db::create_mcp_server_config(
            &pool,
            "oauth-test-server",
            &base_url,
            &HashMap::new(),
            "oauth",
            None,
            None,
        )
        .await
        .expect("create mcp server config");
        db::set_mcp_server_oauth_credentials(
            &pool,
            created.id,
            Some(serde_json::json!({"client_id": "abc"})),
        )
        .await
        .expect("seed oauth credentials");

        disconnect(&pool, &created)
            .await
            .expect("disconnect should succeed");

        let after = db::get_mcp_server_config(&pool, created.id)
            .await
            .expect("get mcp server config")
            .expect("row should exist");
        assert!(after.oauth_credentials.is_none());
    }
}
