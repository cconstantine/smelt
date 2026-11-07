//! Turns on a Claude Code provider that smelt compacts (SME-431, SME-399's
//! B2): the summary through a one-shot CLI call, the session reset, and
//! the next call seeded from the summary.

use super::*;

/// The window the CLI reports (`result.modelUsage[model].contextWindow`)
/// is learned onto the model row, with its output cap.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_clis_reported_window_is_learned(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut lines = api_call(10, vec![text_block("hi")], "end_turn", 3);
    let mut result = crate::claude_cli::tests::fake::result_ok();
    result["modelUsage"] = json!({"claude-opus-5-5": {"contextWindow": 200_000, "maxOutputTokens": 64_000}});
    lines.push(result);
    let fake = FakeCli::new("b2-window", vec![vec![json!({"read": true}), json!({"emit": lines})]]);
    let provider = fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("a turn");

    let (window, cap): (Option<i32>, Option<i32>) = sqlx::query_as("SELECT reported_context_window, reported_max_output FROM provider_models WHERE provider_id = $1 AND model = 'claude-opus-5-5'")
        .bind(provider)
        .fetch_one(&pool)
        .await
        .expect("the model row");
    assert_eq!((window, cap), (Some(200_000), Some(64_000)));
}
