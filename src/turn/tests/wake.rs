use super::*;

#[sqlx::test]
async fn test_wake_conversation_is_a_noop_when_nothing_is_pending(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let counter = start_mock_upstream(&pool, vec!["unused".to_string()]).await;

    let result = wake_conversation(&pool, conversation.id)
        .await
        .expect("wake_conversation should succeed even with nothing pending");
    assert!(
        result.is_empty(),
        "expected no persisted messages, got {result:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "nothing pending should mean no API call at all"
    );

    let messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert!(
        messages.is_empty(),
        "no message should be persisted when nothing is pending"
    );
}

#[sqlx::test]
async fn test_wake_conversation_drains_a_pending_command_and_completes_a_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;

    let messages = wake_conversation(&pool, conversation.id)
        .await
        .expect("wake_conversation should succeed");

    assert_eq!(
        messages.len(),
        2,
        "expected the notification plus the assistant's reply, got {messages:?}"
    );
    assert_eq!(messages[0].role, "user");
    assert_eq!(
        messages[0].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Terminal command cmd-1 finished: exit code 0.".to_string()
        }]
    );
    assert_eq!(messages[1].role, "assistant");

    let remaining = db::unnotified_finished_terminal_commands(&pool, conversation.id)
        .await
        .expect("query unnotified commands");
    assert!(
        remaining.is_empty(),
        "the command should now be marked notified"
    );
}

#[sqlx::test]
async fn test_wake_conversation_second_call_is_a_noop_once_the_first_drained_everything(
    pool: PgPool,
) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    let counter = start_mock_upstream(&pool, vec![body]).await;

    let first = wake_conversation(&pool, conversation.id)
        .await
        .expect("first wake should succeed");
    assert_eq!(first.len(), 2);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "first wake should make exactly one API call"
    );

    // Simulates a second, near-simultaneous exit event's own detached
    // wake_conversation call — nothing should be left to drain, so this
    // must not persist another message or make another API call.
    let second = wake_conversation(&pool, conversation.id)
        .await
        .expect("second wake should succeed");
    assert!(
        second.is_empty(),
        "second wake should find nothing left to drain, got {second:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "second wake should not make another API call"
    );
}

/// SME-72: with no model to run on, a wake-up still saves the finished
/// command's notice (the model sees it once there is one) and says why
/// it couldn't deliver it; a wake with nothing pending doesn't take the
/// default or say anything.
#[sqlx::test]
async fn test_a_wake_with_no_model_still_saves_the_notice(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;
    let mut rx = events::subscribe(conversation.id);

    let error = wake_conversation(&pool, conversation.id)
        .await
        .expect_err("no model to deliver to");

    assert_eq!(error.message(), crate::providers::NO_MODEL_CONFIGURED);
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    assert_eq!(saved.len(), 1, "the notice is saved: {saved:?}");
    let failure = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let events::ConversationEvent::NotificationDeliveryFailed { detail } =
                rx.recv().await.expect("event channel should not close")
            {
                return detail;
            }
        }
    })
    .await
    .expect("the failure is published");
    assert_eq!(failure, crate::providers::NO_MODEL_CONFIGURED);
}

#[sqlx::test]
async fn test_a_wake_with_nothing_pending_leaves_the_model_alone(pool: PgPool) {
    let _guard = lock_turn_tests();
    // The mock first: it moves conversations already there onto it.
    start_mock_upstream(&pool, vec!["unused".to_string()]).await;
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    wake_conversation(&pool, conversation.id).await.expect("nothing to do");

    let model = db::get_conversation_model(&pool, conversation.id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(model.provider_id, None, "a wake with nothing to say doesn't take the default");
}

#[sqlx::test]
async fn test_wake_conversation_publishes_notification_delivery_failed_on_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    // `fail_count` is irrelevant when `success_body` is `None` — every
    // request fails regardless (see the helper's doc comment).
    start_mock_upstream_failing_n_times(&pool, 0, None).await;

    let mut rx = events::subscribe(conversation.id);

    let result = wake_conversation(&pool, conversation.id).await;
    assert!(
        result.is_err(),
        "expected wake_conversation to surface the underlying failure, got {result:?}"
    );

    // The turn publishes other events too (its state, the drained
    // notice, reply resets); wait for this one.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await.expect("event channel should not close") {
                event @ events::ConversationEvent::NotificationDeliveryFailed { .. } => return event,
                _ => continue,
            }
        }
    })
    .await
    .expect("should not time out waiting for the event");
    assert!(
        matches!(
            event,
            events::ConversationEvent::NotificationDeliveryFailed { .. }
        ),
        "expected NotificationDeliveryFailed, got {event:?}"
    );

    // The notification text itself is durably persisted regardless —
    // the drain step commits before the API call that failed.
    let messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert_eq!(
        messages.len(),
        1,
        "the notification message should still be persisted, got {messages:?}"
    );
    assert_eq!(messages[0].role, "user");
}
