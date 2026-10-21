//! SME-177 B23: a callback ends the pending sign-in only when it carries
//! that sign-in's `state`. Anyone can send a request to the callback route
//! (an `<img>` will do), so one without the state must leave the user's
//! Connect pending.

use super::*;
use super::callback_errors::callback_location;

/// A code callback with `state` absent.
fn code_callback_without_state() -> CallbackParams {
    CallbackParams {
        state: None,
        ..code_callback("unused", None)
    }
}

/// The genuine callback for `state` still completes the sign-in.
async fn assert_sign_in_completes(pool: &PgPool, server_id: i64, state: &str, case: &str) {
    complete_callback(server_id, code_callback(state, None))
        .await
        .unwrap_or_else(|e| panic!("{case}: the genuine callback should complete: {e}"));
    assert!(
        stored_credentials(pool, server_id).await.is_some(),
        "{case}: the genuine callback's grant should be saved"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_forged_code_callback_leaves_the_sign_in_pending(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let state = start_attempt(&pool, &config).await;

    let location = callback_location(config.id, code_callback("forged", None)).await;
    assert_eq!(location, format!("/mcp-servers/{}?oauth_callback=1", config.id));

    assert_sign_in_completes(&pool, config.id, &state, "after a forged state").await;
}

/// Passes on the unfixed code too, since a missing `state` returns before
/// `PENDING` is touched; a guard against moving the take before that check.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_code_callback_without_state_leaves_the_sign_in_pending(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let state = start_attempt(&pool, &config).await;

    callback_location(config.id, code_callback_without_state()).await;

    assert_sign_in_completes(&pool, config.id, &state, "after a callback without state").await;
}

/// Attempt A completes; a second Connect starts S′; A's callback, sent
/// again, must not end S′.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_replayed_callback_leaves_the_next_sign_in_pending(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let first = start_attempt(&pool, &config).await;
    assert_sign_in_completes(&pool, config.id, &first, "the first sign-in").await;
    let second = start_attempt(&pool, &config).await;

    complete_callback(config.id, code_callback(&first, None))
        .await
        .expect_err("a replayed callback is refused");

    assert_sign_in_completes(&pool, config.id, &second, "after a replay").await;
}

/// S is replaced by a second Connect's S′ before its callback arrives;
/// S's callback must not end S′.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_callback_for_a_replaced_attempt_leaves_the_new_one_pending(pool: PgPool) {
    let base_url = spawn_mock_oauth_server(MockMetadata::None).await;
    let config = create_oauth_server(&pool, &base_url).await;
    let replaced = start_attempt(&pool, &config).await;
    let current = start_attempt(&pool, &config).await;

    complete_callback(config.id, code_callback(&replaced, None))
        .await
        .expect_err("a replaced attempt's callback is refused");

    assert_sign_in_completes(&pool, config.id, &current, "after the replaced attempt's callback")
        .await;
}
