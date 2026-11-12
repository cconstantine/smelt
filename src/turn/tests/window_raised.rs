//! SME-423 B: the model row's Save forgets what was learned about its
//! window — the way a stuck conversation lifts when the window was raised
//! on the server itself, where nothing smelt reads has changed. The lift
//! itself is `CompactionProgress::sync`'s (pinned by
//! `test_a_new_window_forgets_what_was_learned`); this is the save
//! reaching that change.

use super::compaction_progress::{step, stepping_mock};
use super::window_stop::set_learned_context_window;
use super::*;

/// A turn that can't fit against a learned 20,000 sticks its wakes; the
/// row's Save (`set_model_settings` with nothing in it) forgets the
/// learned number, and the next wake calls the model and succeeds at the
/// window that takes its place.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_saving_a_models_row_lifts_a_stuck_conversation(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(12_000, 2_000); 30]).await;
    set_learned_context_window(&pool, 20_000).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("it can't fit");
    let calls = seen.count();

    let provider_id: i64 = sqlx::query_scalar("SELECT id FROM inference_providers").fetch_one(&pool).await.expect("the mock provider");
    crate::providers::set_model_settings(&pool, provider_id, crate::providers::test_support::MOCK_MODEL, crate::providers::ModelSettings::default())
        .await
        .expect("the row's Save");

    unnotified_finished_command(&pool, conversation.id, "sme-423-t1").await;
    wake_conversation(&pool, conversation.id).await.expect("the save's clear lifted the stuck");
    // `> calls`, not `calls + 1`: the mock's 30 step answers keep the
    // freed turn stepping, and "the model was called" is the fact — the
    // stuck wake before the save called nothing at all.
    assert!(seen.count() > calls, "the wake called the model");
}
