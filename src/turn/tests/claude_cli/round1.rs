//! Turns on a Claude Code provider: the cases PR #291's round-1 review
//! (SME-431) added.

use super::*;

/// The `xmin` of this provider's model row: it changes on any write.
async fn row_version(pool: &PgPool, provider_id: i64) -> String {
    sqlx::query_scalar("SELECT xmin::text FROM provider_models WHERE provider_id = $1 AND model = 'claude-opus-5-5'")
        .bind(provider_id)
        .fetch_one(pool)
        .await
        .expect("the model row")
}

/// A reply whose `result` reports `window` for the model.
fn reply_reporting(window: i64) -> Value {
    let mut lines = api_call(10, vec![text_block("hi")], "end_turn", 3);
    let mut result = crate::claude_cli::tests::fake::result_ok();
    result["modelUsage"] = json!({"claude-opus-5-5": {"contextWindow": window, "maxOutputTokens": 64_000}});
    lines.push(result);
    json!({"emit": lines})
}

/// Whether `rx` has had `ProvidersChanged` since it was last drained.
fn providers_changed(rx: &mut tokio::sync::broadcast::Receiver<crate::events::AppEvent>) -> bool {
    let mut seen = false;
    while let Ok(event) = rx.try_recv() {
        seen |= event == crate::events::AppEvent::ProvidersChanged;
    }
    seen
}

/// L5: a window the CLI reports tells the app (the providers page and
/// pickers refresh), and the same window again writes nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reported_window_tells_the_app_and_isnt_rewritten(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let fake = FakeCli::new("r1-l5", vec![
        vec![json!({"read": true}), reply_reporting(200_000)],
        vec![json!({"read": true}), reply_reporting(200_000)],
    ]);
    let provider = fake.pick_for(&pool, conversation.id).await;
    let mut app = crate::events::subscribe_app();

    run_turn(&pool, conversation.id, hello()).await.expect("turn 1");
    assert!(providers_changed(&mut app), "the new window tells the app");
    let version = row_version(&pool, provider).await;

    run_turn(&pool, conversation.id, hello()).await.expect("turn 2");
    assert_eq!(fake.started(), 2, "a run each turn, each reporting the window");
    assert_eq!(row_version(&pool, provider).await, version, "the same window writes nothing");
}
