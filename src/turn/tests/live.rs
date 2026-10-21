use super::*;

/// Every tab can show a reply as it streams, one bubble per model
/// call: a reset when each call starts, then its text.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_streams_its_reply_to_every_tab(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_007)
        .await
        .expect("create conversation");
    start_mock_upstream(&pool, text_tool_then_text_bodies()).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let streamed: Vec<String> = drain_events(&mut rx)
        .await
        .into_iter()
        .filter_map(|event| match event {
            events::ConversationEvent::ReplyReset {} => Some("<reset>".to_string()),
            events::ConversationEvent::ReplyDelta { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, vec!["<reset>", "Adding.", "<reset>", "Sum is 5"]);
    assert_eq!(reply_in_progress(conversation.id), None, "nothing in progress once the turn ends");
}

/// A tab that connects mid-reply can show the text so far.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_reply_so_far_is_available_mid_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_008)
        .await
        .expect("create conversation");
    start_partial_then_hanging_mock_upstream(&pool, "Partial answer").await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    let seen = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(text) = reply_in_progress(conversation.id) {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the partial reply should be available mid-turn");
    assert_eq!(seen, "Partial answer");

    stop_turn_now(conversation.id);
    let _ = turn.await;
    assert_eq!(reply_in_progress(conversation.id), None, "a stopped turn leaves nothing in progress");
}

/// Sending returns at once; the turn carries on and every tab hears it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_start_turn_returns_before_the_turn_finishes(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_010)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(conversation.id);

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        start_turn(pool.clone(), conversation.id, "hello there".to_string()),
    )
    .await
    .expect("sending shouldn't wait for the model")
    .expect("sending should succeed");

    let user_message = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(events::ConversationEvent::MessagesAppended { messages }) = rx.recv().await {
                return messages;
            }
        }
    })
    .await
    .expect("the user's message should be published");
    assert_eq!(user_message.len(), 1);
    assert!(user_message[0].content.contains("hello there"));
    assert!(turn_running(conversation.id), "the turn should still be running");
    stop_turn_now(conversation.id);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_start_turn_refuses_a_missing_conversation(pool: PgPool) {
    let error = start_turn(pool, 9_100_000_011, "hi".to_string())
        .await
        .expect_err("a conversation that doesn't exist")
        .to_string();
    assert!(error.contains("conversation not found"), "got: {error}");
}

/// A sent turn that fails, or is stopped, tells every tab why.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_sent_turn_publishes_its_failure(pool: PgPool) {
    let _guard = lock_turn_tests();
    let failing = db::create_conversation_with_id(&pool, 9_100_000_012)
        .await
        .expect("create conversation");
    start_mock_upstream_failing_n_times(&pool, 0, None).await;
    let mut rx = events::subscribe(failing.id);
    start_turn(pool.clone(), failing.id, "hi".to_string()).await.expect("send");
    let error = next_turn_error(&mut rx).await.expect("a TurnError");
    assert!(error.contains("error parsing tool call"), "got: {error}");

    let stopped = db::create_conversation_with_id(&pool, 9_100_000_013)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(stopped.id);
    start_turn(pool.clone(), stopped.id, "hi".to_string()).await.expect("send");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop_turn_now(stopped.id);
    assert_eq!(next_turn_error(&mut rx).await.as_deref(), Some(TURN_STOPPED));
}

/// SME-51 B11: a failed turn's error was only an event, so a tab that
/// reloaded (or connected) afterwards showed a message with no reply
/// and no reason. The last error is kept for the reconnect pull, and
/// the user's next message clears it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_error_is_still_there_after_a_reload(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_014)
        .await
        .expect("create conversation");
    start_mock_upstream_failing_n_times(&pool, 0, None).await;
    let mut rx = events::subscribe(conversation.id);
    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    next_turn_error(&mut rx).await.expect("a TurnError");
    let kept = last_turn_error(conversation.id).expect("the error is kept");
    assert!(kept.contains("error parsing tool call"), "got: {kept}");

    start_hanging_mock_upstream(&pool).await;
    start_turn(pool.clone(), conversation.id, "again".to_string()).await.expect("send");
    assert_eq!(last_turn_error(conversation.id), None, "the next message clears it");
    stop_turn_now(conversation.id);
    // The stop pauses the conversation for the rest of the process; a
    // later test on this id would find its wakes starting no turn.
    resume_turns(conversation.id);
}

/// SME-51 code review 1: a turn the user didn't send (a finished
/// task's, say) that goes fine replaces an earlier failure; a reload
/// mustn't show that error under the newer reply.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_any_new_turn_clears_the_kept_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_015)
        .await
        .expect("create conversation");
    remember_turn_error(conversation.id, Some("an earlier failure".to_string()));
    start_recording_mock_upstream(&pool, vec![text_reply_body("Done.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("a background turn");
    assert_eq!(last_turn_error(conversation.id), None);
}

async fn next_turn_error(
    rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>,
) -> Option<String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::TurnError { message }) => return Some(message),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// Stopping a turn a finished command started isn't a failed
/// notification.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_stopping_a_woken_turn_is_not_reported_as_a_failure(pool: PgPool) {
    let _guard = lock_turn_tests();
    // Not `test_a_turn_error_is_still_there_after_a_reload`'s id: that
    // test's stop leaves its conversation paused for the rest of the
    // process, and a wake there starts no turn (SME-135).
    let conversation = db::create_conversation_with_id(&pool, 9_135_000_014)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-woken").await;
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(conversation.id);

    let wake = tokio::spawn({
        let pool = pool.clone();
        async move { wake_conversation(&pool, conversation.id).await }
    });
    // Stop once the turn has started, not after a fixed wait: a stop
    // landing before the wake checks for a pause leaves nothing to stop,
    // and a wake that never started a turn (its conversation left paused
    // by another test's stop, say) would pass the check below untested.
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let events::ConversationEvent::TurnState { running: true, .. } =
                rx.recv().await.expect("event channel should not close")
            {
                return;
            }
        }
    })
    .await;
    assert!(started.is_ok(), "the wake should have started a turn");
    stop_turn_now(conversation.id);
    let _ = wake.await;

    let reported = drain_events(&mut rx)
        .await
        .into_iter()
        .any(|e| matches!(e, events::ConversationEvent::NotificationDeliveryFailed { .. }));
    assert!(!reported, "a stop isn't a failed notification");
    // As above: don't leave the conversation paused for later tests.
    resume_turns(conversation.id);
}

/// Every tab sees each message as it's saved: the user's own first,
/// then each step of a multi-step reply, not one batch at the end.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_publishes_each_message_as_it_is_saved(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_006)
        .await
        .expect("create conversation");
    start_mock_upstream(&pool, text_tool_then_text_bodies()).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let batches: Vec<Vec<String>> = drain_events(&mut rx)
        .await
        .into_iter()
        .filter_map(|event| match event {
            events::ConversationEvent::MessagesAppended { messages } => {
                Some(messages.into_iter().map(|m| m.role).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        batches,
        vec![
            vec!["user".to_string()],
            vec!["assistant".to_string()],
            vec!["user".to_string()],
            vec!["assistant".to_string()],
        ],
        "one event per saved message, in order"
    );
}
