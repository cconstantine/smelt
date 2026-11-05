//! SME-386 F10: a turn can't keep compacting without getting anywhere. A
//! compaction that leaves the next request over the trigger, or that frees
//! a little each time, ends the turn and says why; one that frees real room
//! goes on as often as the turn needs it.

use super::ask_user::tool_calls_body;
use super::overflow::{compaction_summaries, start_refusal_mock, too_long};
use super::window_stop::set_context_window;
use super::*;

/// A step that calls `todoread` (cheap, database only), measuring `input`
/// tokens in and `output` out.
fn step(input: i64, output: i64) -> String {
    with_usage(&tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]), input, 0, output)
}

/// What the model answers: each of `steps`, with a summary after every one
/// (the call a compaction makes), then a final reply.
fn alternating(steps: Vec<String>) -> Vec<String> {
    let mut bodies = Vec::new();
    for step in steps {
        bodies.push(step);
        bodies.push(text_reply_body("The summary."));
    }
    bodies.push(text_reply_body("Done."));
    bodies
}

async fn summaries(pool: &PgPool, id: i64) -> usize {
    compaction_summaries(pool, id).await.len()
}

/// The finding's own shape: the prompt alone is over the trigger, so the
/// compaction can't get under it and every step would compact again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_prompt_over_the_trigger_compacts_once_and_ends_the_turn_saying_why(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // The ceiling at 28,000 is 11,616, and F10 measured about 12,380.
    start_recording_mock_upstream(&pool, alternating(vec![step(12_380, 100); 30])).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn_bounded(&pool, TurnInFlight::start(conversation.id), Some(hello()), 12, None, false)
        .await
        .expect_err("the turn can't get anywhere");

    let error = error.to_string();
    assert_eq!(summaries(&pool, conversation.id).await, 1, "one compaction showed it can't help: {error}");
    assert!(error.contains("(28,000 tokens) is too small for this conversation"), "{error}");
    assert!(error.contains("about 12,380 tokens"), "it names what a compacted request measured: {error}");
    assert!(error.contains("16,384"), "it names what's kept for the reply: {error}");
}

/// A compaction that does free a little each time: the request after it is
/// under the trigger, by less than a step takes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_compactions_that_each_free_a_little_end_the_turn_saying_why(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    start_recording_mock_upstream(&pool, alternating(vec![step(11_000, 700); 30])).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn_bounded(&pool, TurnInFlight::start(conversation.id), Some(hello()), 30, None, false)
        .await
        .expect_err("the turn can't get anywhere");

    let error = error.to_string();
    assert_eq!(summaries(&pool, conversation.id).await, 3, "{error}");
    assert!(error.contains("(28,000 tokens) is too small for this conversation"), "{error}");
}

/// A long ask whose steps read big outputs (F11's nine steps, 11 compactions
/// at 40,000): each compaction frees what the last step brought, so the turn
/// is getting somewhere however often it compacts.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_compactions_that_free_real_room_go_on_as_often_as_needed(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    start_recording_mock_upstream(&pool, alternating(vec![step(15_000, 9_000); 8])).await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("every compaction freed real room");

    assert_eq!(summaries(&pool, conversation.id).await, 8);
}

/// A low yield now and then isn't a loop: only a run of them ends a turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_that_frees_real_room_starts_the_count_again(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // Over the 23,616 ceiling after each step, and a compaction's yield is
    // read off the step after it: about 700 or 1,000 tokens twice, then a
    // real 17,000, round and round.
    let round = [step(23_000, 700), step(23_000, 9_000), step(15_000, 9_000)];
    let steps: Vec<String> = round.iter().cycle().take(9).cloned().collect();
    start_recording_mock_upstream(&pool, alternating(steps)).await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("no run of three low yields");

    assert_eq!(summaries(&pool, conversation.id).await, 9);
}

/// The user's turn says why in the conversation, durably: a reload shows it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_sent_turn_saves_why_it_ended(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    start_recording_mock_upstream(&pool, alternating(vec![step(12_380, 100); 6])).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mut rx = events::subscribe(conversation.id);

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;

    let reason = last_failure_reason(&pool, conversation.id).await.expect("the conversation says why");
    assert!(reason.contains("is too small for this conversation"), "{reason}");
}

/// A compaction answering a refusal counts like a proactive one: the window
/// the refusal named is still too small for what the compaction left.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_answering_a_refusal_counts_too(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut answers = vec![too_long(30_000, 40_000)];
    for body in [text_reply_body("The summary."), step(25_000, 100), text_reply_body("The summary."), step(25_000, 100)] {
        answers.push(Ok(body));
    }
    answers.push(Ok(text_reply_body("Done.")));
    start_refusal_mock(&pool, answers).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the compacted request is over the trigger");

    let error = error.to_string();
    assert_eq!(summaries(&pool, conversation.id).await, 1, "{error}");
    assert!(error.contains("(40,000 tokens) is too small for this conversation"), "{error}");
}
