use super::*;

use std::sync::atomic::{AtomicI64, Ordering};

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::routing::{get, post};

/// SME-113 review 1: GitHub's refusals, as its docs give them
/// (`error_description` and `error_uri` included), become RFC 6749's
/// 400; a token, and a reply that isn't a 200, pass untouched.
#[test]
fn test_a_token_error_sent_with_a_200_becomes_a_400_invalid_grant() {
    let github = br#"{"error":"bad_refresh_token","error_description":"The refresh token passed is incorrect or expired.","error_uri":"https://docs.github.com"}"#;
    let fixed = as_token_error(http::StatusCode::OK, github).expect("a refusal");
    let fixed: serde_json::Value = serde_json::from_slice(&fixed).expect("json");
    assert_eq!(fixed["error"], "invalid_grant");
    assert_eq!(fixed["error_description"], "The refresh token passed is incorrect or expired.");

    let other = br#"{"error":"incorrect_client_credentials"}"#;
    let fixed = as_token_error(http::StatusCode::OK, other).expect("a refusal");
    assert!(String::from_utf8_lossy(&fixed).contains("incorrect_client_credentials"));

    let token = br#"{"access_token":"t","token_type":"bearer"}"#;
    assert_eq!(as_token_error(http::StatusCode::OK, token), None);
    assert_eq!(as_token_error(http::StatusCode::BAD_REQUEST, github), None);
    assert_eq!(as_token_error(http::StatusCode::OK, b"not json"), None);
}

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
async fn token_handler(
    State(gate): State<Option<Arc<TokenGate>>>,
    body: Bytes,
) -> Json<serde_json::Value> {
    if let Some(gate) = gate {
        gate.arrived.notify_one();
        gate.release.notified().await;
    }
    let body = String::from_utf8_lossy(&body);
    let is_refresh = body.contains("grant_type=refresh_token");
    Json(serde_json::json!({
        "access_token": if is_refresh { "refreshed-access-token" } else { "initial-access-token" },
        "token_type": "bearer",
        "expires_in": if is_refresh { 3600 } else { 0 },
        "refresh_token": "test-refresh-token",
    }))
}

/// Holds the mock's token endpoint open (SME-166), so a test can change
/// the server's row while a code exchange is in flight: the handler says
/// it `arrived`, then waits for `release`. `Notify` keeps one permit, so
/// neither side can miss the other.
#[derive(Default)]
struct TokenGate {
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
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
    /// `authorization_endpoint` is a `javascript:` URL (B4, SME-122).
    JavascriptEndpoint,
}

/// The issuer the mock at `base_url` advertises — what a callback's
/// `iss` must equal.
fn mock_issuer(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

async fn spawn_mock_oauth_server(metadata: MockMetadata) -> String {
    spawn_gated_mock_oauth_server(metadata, None).await
}

async fn spawn_gated_mock_oauth_server(
    metadata: MockMetadata,
    gate: Option<Arc<TokenGate>>,
) -> String {
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
        MockMetadata::JavascriptEndpoint => (Some(mock_issuer(&base_url)), true),
        MockMetadata::WrongIssuer => (Some("http://127.0.0.1:1".to_string()), true),
    };
    let authorization_endpoint = match metadata {
        MockMetadata::JavascriptEndpoint => {
            "javascript:document.body.textContent='B4-RAN'//".to_string()
        }
        _ => format!("{base_url}authorize"),
    };
    if let Some(issuer) = issuer {
        let document = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": authorization_endpoint,
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
    let app = app.with_state(gate);
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

/// B4 (SME-122): an authorization server whose metadata names a
/// `javascript:` authorization endpoint gets no URL handed to the
/// browser, and leaves no attempt pending.
#[sqlx::test]
async fn test_a_javascript_authorization_endpoint_is_refused(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::JavascriptEndpoint).await;
    let config = create_oauth_server(&pool, &base_url).await;

    let result = start(
        &pool,
        &config,
        "http://localhost/oauth/mcp-callback/1".to_string(),
    )
    .await;

    let error = result.expect_err("a javascript: authorization endpoint must be refused");
    assert!(
        error.contains("http or https"),
        "the error should say what's wrong with the URL: {error}"
    );
    assert!(
        !PENDING.lock().await.contains_key(&config.id),
        "a refused start must leave no attempt pending"
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
    manager.set_credential_store(PgCredentialStore::for_refreshes(pool.clone(), created.id));
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

async fn change_url(pool: &PgPool, config: &McpServerConfig, name: &str, url: &str) {
    db::update_mcp_server_config(pool, config.id, name, url, &HashMap::new(), &[], "oauth")
        .await
        .expect("update mcp server config")
        .expect("row should exist");
}

/// A URL edit after Connect: the token issued for the old address isn't
/// saved onto the new one, and the error says why.
#[sqlx::test]
async fn test_a_callback_after_a_url_edit_saves_nothing(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let state = start_attempt(&pool, &config).await;

    change_url(&pool, &config, &config.name, "http://127.0.0.1:1/elsewhere").await;
    let error = complete_callback(config.id, code_callback(&state, None))
        .await
        .expect_err("a sign-in for the old address must not complete");

    assert_eq!(error, ADDRESS_CHANGED_ERROR, "the page shows the reason alone");
    assert!(
        stored_credentials(&pool, config.id).await.is_none(),
        "the old address's grant must not be saved onto the new URL"
    );
}

/// The interleaving that dropping `PENDING` on an edit can't cover: the
/// callback has already taken its attempt and is waiting on the
/// provider's token endpoint when the URL changes.
#[sqlx::test]
async fn test_a_url_edit_during_the_code_exchange_saves_nothing(pool: PgPool) {
    let gate = Arc::new(TokenGate::default());
    let base_url = spawn_gated_mock_oauth_server(MockMetadata::None, Some(gate.clone())).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let state = start_attempt(&pool, &config).await;

    let id = config.id;
    let callback =
        tokio::spawn(async move { complete_callback(id, code_callback(&state, None)).await });
    gate.arrived.notified().await;
    assert!(
        !PENDING.lock().await.contains_key(&config.id),
        "the callback has taken its attempt before the exchange"
    );
    change_url(&pool, &config, &config.name, "http://127.0.0.1:1/elsewhere").await;
    gate.release.notify_one();
    let error = callback
        .await
        .expect("callback task")
        .expect_err("a sign-in for the old address must not complete");

    assert_eq!(error, ADDRESS_CHANGED_ERROR, "the page shows the reason alone");
    assert!(
        stored_credentials(&pool, config.id).await.is_none(),
        "the old address's grant must not be saved onto the new URL"
    );
}

#[sqlx::test]
async fn test_a_callback_after_the_server_is_deleted_saves_nothing(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let state = start_attempt(&pool, &config).await;

    db::delete_mcp_server_config(&pool, config.id)
        .await
        .expect("delete mcp server config");
    let error = complete_callback(config.id, code_callback(&state, None))
        .await
        .expect_err("a sign-in for a deleted server must not complete");

    assert_eq!(error, ADDRESS_CHANGED_ERROR, "the page shows the reason alone");
    assert!(
        db::get_mcp_server_config(&pool, config.id)
            .await
            .expect("get mcp server config")
            .is_none(),
        "the deleted server must stay deleted"
    );
}

/// The guard is on the URL alone: a rename, or an edit that puts the URL
/// back, leaves the sign-in valid for the address the row now has.
#[sqlx::test]
async fn test_a_callback_after_edits_that_leave_the_url_as_it_was_saves(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    for case in ["a rename", "a URL changed and changed back"] {
        let config = create_oauth_server(&pool, &base_url).await;
        let state = start_attempt(&pool, &config).await;

        if case == "a rename" {
            change_url(&pool, &config, "renamed", &config.url).await;
        } else {
            change_url(&pool, &config, &config.name, "http://127.0.0.1:1/elsewhere").await;
            change_url(&pool, &config, &config.name, &config.url).await;
        }
        complete_callback(config.id, code_callback(&state, None))
            .await
            .unwrap_or_else(|e| panic!("case {case}: the sign-in should complete: {e}"));

        assert!(
            stored_credentials(&pool, config.id).await.is_some(),
            "case {case}: the grant should be saved"
        );
    }
}
