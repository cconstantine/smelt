//! SME-168: a failed callback's error is kept on the server and taken once
//! by the edit page, never carried in the redirect's URL.

use super::*;

/// `CALLBACK_ERRORS` is shared by every test in the process; ids of their
/// own keep these tests apart from each other and from `create_oauth_server`'s.
fn unused_server_id() -> i64 {
    static NEXT_ID: AtomicI64 = AtomicI64::new(20_000_000);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn expired() -> Instant {
    Instant::now()
        .checked_sub(CALLBACK_ERROR_TTL + Duration::from_secs(1))
        .expect("the clock is past the time to live")
}

#[test]
fn test_one_servers_callback_error_is_not_another_servers() {
    let (a, b) = (unused_server_id(), unused_server_id());

    record_callback_error(a, "server A's error".to_string());

    assert_eq!(take_callback_error(b), None, "B has nothing kept");
    assert_eq!(
        take_callback_error(a).as_deref(),
        Some("server A's error"),
        "A's error is kept for A"
    );
    assert_eq!(take_callback_error(a), None, "a take empties the slot");
}

#[test]
fn test_a_later_callback_error_replaces_the_earlier_one() {
    let id = unused_server_id();

    record_callback_error(id, "first".to_string());
    record_callback_error(id, "second".to_string());

    assert_eq!(take_callback_error(id).as_deref(), Some("second"));
}

#[test]
fn test_an_expired_callback_error_is_refused_and_pruned() {
    let (old, pruned, fresh) = (unused_server_id(), unused_server_id(), unused_server_id());

    record_callback_error_at(old, "too old".to_string(), expired());
    assert_eq!(take_callback_error(old), None, "an expired error isn't shown");

    record_callback_error_at(pruned, "too old".to_string(), expired());
    record_callback_error(fresh, "new".to_string());
    let errors = callback_errors();
    assert!(
        !errors.contains_key(&pruned),
        "the next record prunes an expired entry"
    );
    assert!(errors.contains_key(&fresh), "the new entry is kept");
}

/// Sends `params` through the real route handler and returns the
/// `Location` it redirects to.
async fn callback_location(id: i64, params: CallbackParams) -> String {
    use axum::response::IntoResponse;
    let response = callback_handler(axum::extract::Path(id), axum::extract::Query(params))
        .await
        .into_response();
    response
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("the callback redirects")
        .to_str()
        .expect("an ASCII location")
        .to_string()
}

const MARKER: &str = "Your session expired. Call support at 555-0100";

fn error_callback_with_marker(state: Option<&str>, iss: Option<&str>) -> CallbackParams {
    CallbackParams {
        error_description: Some(MARKER.to_string()),
        ..error_callback(state, iss)
    }
}

#[sqlx::test]
async fn test_a_verified_error_callbacks_text_stays_out_of_the_redirect(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::Issuer {
        iss_supported: true,
    })
    .await;
    let config = create_oauth_server(&pool, &base_url).await;
    let state = start_attempt(&pool, &config).await;

    let location = callback_location(
        config.id,
        error_callback_with_marker(Some(&state), Some(&mock_issuer(&base_url))),
    )
    .await;

    assert_eq!(location, format!("/mcp-servers/{}?oauth_callback=1", config.id));
    assert_eq!(take_callback_error(config.id).as_deref(), Some(MARKER));
    assert_eq!(take_callback_error(config.id), None, "it shows once");
}

#[sqlx::test]
async fn test_a_forged_error_callback_keeps_only_the_unverified_error(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;

    let location = callback_location(
        config.id,
        error_callback_with_marker(Some("forged"), None),
    )
    .await;

    assert_eq!(location, format!("/mcp-servers/{}?oauth_callback=1", config.id));
    assert_eq!(take_callback_error(config.id).as_deref(), Some(UNVERIFIED_ERROR));
}

/// SME-166's refusal reaches the page through the same slot.
#[sqlx::test]
async fn test_a_callback_after_a_url_edit_keeps_the_address_changed_error(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let state = start_attempt(&pool, &config).await;
    change_url(&pool, &config, &config.name, "http://127.0.0.1:1/elsewhere").await;

    let location = callback_location(config.id, code_callback(&state, None)).await;

    assert_eq!(location, format!("/mcp-servers/{}?oauth_callback=1", config.id));
    assert_eq!(
        take_callback_error(config.id).as_deref(),
        Some(ADDRESS_CHANGED_ERROR)
    );
}

#[sqlx::test]
async fn test_a_successful_callback_clears_an_earlier_error(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    record_callback_error(config.id, "an earlier failed sign-in".to_string());
    let state = start_attempt(&pool, &config).await;

    let location = callback_location(config.id, code_callback(&state, None)).await;

    assert_eq!(location, format!("/mcp-servers/{}", config.id));
    assert!(stored_credentials(&pool, config.id).await.is_some());
    assert_eq!(take_callback_error(config.id), None, "success leaves nothing to show");
}
