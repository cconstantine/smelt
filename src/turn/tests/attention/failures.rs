//! The attention tests' failure kinds: CutOff, Refused, Failed, and the
//! SME-73 overflow pair (split from `attention.rs`, SME-377).

use super::*;

/// A reply cut off at its budget gets the notice, and the spell ends
/// CutOff even though the turn itself ended Ok (the maximum at work:
/// CutOff is noted at the reply, whatever the turn's end).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_reply_publishes_cutoff(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let body = text_reply_body("half a reply").replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_mock_upstream(&pool, vec![body]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::CutOff);
}

/// A refused reply, same shape: Refused.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_reply_publishes_refused(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let body = text_reply_body("I can't help with that").replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"refusal""#);
    start_mock_upstream(&pool, vec![body]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Refused);
}

/// A user's failed turn — its failure notice saved, so it ends
/// `TurnFailure::Noted` — still means Failed. (The plan's mutation:
/// matching only `Failed` misses this one.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_user_turn_failing_with_its_notice_saved_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one NeedsUser, saw {seen:?}");
}

/// A notice's turn failing (`deliver_notice`'s `NotificationDeliveryFailed`
/// case): Failed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failing_notice_turn_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    deliver_notice(&pool, conversation.id, "Terminal command cmd-1 finished: exit code 0.".to_string()).await;
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}

/// A wake's turn failing: Failed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failing_wake_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect_err("the failing call fails the wake");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}

/// `MAX_TURNS`: driven by `run_turn_bounded`'s own bound, a tool-looping
/// reply with `max_turns: 1` fails the turn — and Failed is what the
/// spell ends with.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_exceeding_max_turns_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // Every request asks for a tool call, so the loop never reaches a
    // final reply; one step is all the bound allows.
    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_x","name":"nonexistent_tool","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;
    let recording = events::AppRecording::start();

    let (in_flight, generation) = TurnInFlight::start_by_user(conversation.id);
    let result = run_turn_bounded(&pool, in_flight, Some(hello()), 1, Some(generation), true).await;
    assert!(result.is_err(), "one allowed step can't finish a tool-looping turn");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}

/// The maximum, not the last word: a Failed in a busy spell is what ends
/// it, whatever the model's `notify_user` said earlier in the same spell
/// (T5, SME-377): the message is dropped with the Notified it rode.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_failed_beats_an_earlier_notify_user_in_one_busy_spell(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let recording = events::AppRecording::start();

    // Two turns, one spell: the first notes Notified (the model's call),
    // the second Failed after it (a failing run's end). With a plain
    // overwrite in the other order the spell would end Notified; the
    // maximum keeps Failed.
    let first = TurnInFlight::start(conversation.id);
    note_notified(conversation.id, "All done".to_string());
    let second = TurnInFlight::start(conversation.id);
    note_attention(conversation.id, events::Attention::Failed);
    drop(first);
    assert!(needs_user_of(&recording, conversation.id).is_empty(), "nothing while a turn is still in flight");
    drop(second);
    let generation = turn_generation(conversation.id);
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen, vec![(events::Attention::Failed, generation)]);
    assert_eq!(published_notice(conversation.id, generation), None, "the outranked message isn't kept");
}

/// The SME-73 pair, first half: a turn that overflows once, compacts and
/// answers is a normal ending — the recovery's refused call notes no
/// Failed, so the spell publishes nothing (SME-377: a plain reply notes
/// nothing by itself).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_that_overflows_once_publishes_nothing_not_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    super::overflow::start_refusal_mock(
        &pool,
        vec![
            super::overflow::too_long(120_000, 131_072),
            Ok(text_reply_body("Summary: it was going.")),
            Ok(text_reply_body("Done now.")),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_message(&pool, conversation.id, "user", &[crate::anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::create_message(&pool, conversation.id, "assistant", &[crate::anthropic::ContentBlock::Text { text: "reply".to_string() }])
        .await
        .expect("seed");
    let recording = events::AppRecording::start();

    run_turn(&pool, conversation.id, hello()).await.expect("the refusal recovers through a compaction");
    let seen = published_after_done(&recording, conversation.id).await;
    assert!(seen.is_empty(), "a recovered overflow is a normal ending, no Failed: {seen:?}");
}

/// The SME-73 pair, second half: a turn refused a second time fails —
/// Failed is what the user needs to be told.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_refused_twice_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    super::overflow::start_refusal_mock(&pool, vec![super::overflow::too_long(150_000, 131_072), super::overflow::too_long(150_000, 131_072)])
        .await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_message(&pool, conversation.id, "user", &[crate::anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &crate::anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let recording = events::AppRecording::start();

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the summary refused twice fails the turn");
    assert!(error.to_string().contains("Couldn't compact the conversation"), "{error}");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed, "the second refusal ends the spell Failed");
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one Failed for the spell: {seen:?}");
}
