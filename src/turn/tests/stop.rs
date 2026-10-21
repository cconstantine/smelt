use super::*;

/// Stopping ends an in-flight turn at once, frees the turn lock, and
/// keeps the user's message.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_stop_turn_ends_a_turn_in_flight(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000001)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop_turn_now(conversation.id);

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), turn)
        .await
        .expect("the turn should end within a second of stopping")
        .expect("join");
    let error = result.expect_err("a stopped turn ends with an error").to_string();
    assert!(error.contains(TURN_STOPPED), "got: {error}");
    assert!(
        conversation_lock(conversation.id).try_lock().is_ok(),
        "a stopped turn should release the turn lock"
    );
    let saved = db::list_messages(&pool, conversation.id).await.expect("list");
    assert_eq!(saved.len(), 2, "the user's message is kept, then the stop is noted");
    // SME-51 B10: the stop is in the conversation, for the model (and
    // a compaction) to see that request was called off.
    assert!(
        saved[1].content.contains(STOP_NOTICE),
        "the stop isn't recorded: {}",
        saved[1].content
    );

    // A turn started after the stop isn't affected by it.
    let later = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    run_turn(&pool, conversation.id, hello())
        .await
        .expect("a later turn runs normally");
    assert_eq!(later.lock().expect("log").len(), 1);
}

/// SME-91: a Stop landing just after the turn saved its own message
/// (the INSERT committed, the turn not yet told) doesn't save the
/// message a second time.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_just_after_the_message_is_saved_keeps_it_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000091)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let (reached, release) = test_hooks::pause_after_message_saved(conversation.id);

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the turn saves its message")
        .expect("hook");
    stop_turn_now(conversation.id);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _ = release.send(());

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
        .await
        .expect("the turn ends after the stop")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    let saved = db::list_messages(&pool, conversation.id).await.expect("list");
    let copies = saved.iter().filter(|m| m.content.contains("\"hello\"")).count();
    assert_eq!(copies, 1, "the user's message is saved once: {saved:?}");
    resume_turns(conversation.id);
}

/// SME-91: a stopped reply doesn't stay on screen while another turn
/// is still in flight: the stop clears it and tells every tab.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_clears_the_reply_even_with_another_turn_in_flight(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000092)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    // The turn is waiting on the model by now; its reply has begun.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    relay_reply_delta(conversation.id, "Once upon a ");
    let _another = TurnInFlight::start(conversation.id);
    let mut rx = events::subscribe(conversation.id);

    stop_turn_now(conversation.id);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
        .await
        .expect("the turn ends after the stop");

    assert_eq!(reply_in_progress(conversation.id), None, "the stopped reply is cleared");
    let reset = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match rx.recv().await {
                Ok(events::ConversationEvent::ReplyReset {}) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(reset, "tabs are told to drop the stopped reply");
    resume_turns(conversation.id);
}

/// SME-91: a tab still reconnecting to a deleted conversation (or any
/// that doesn't exist) is refused, and makes no channel for it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_subscribing_to_a_missing_conversation_is_refused(pool: PgPool) {
    let missing = 9_100_000_099;
    assert!(open_conversation_events(&pool, missing).await.is_err());
    assert!(!events::has_channel(missing), "a refused subscription leaves no channel");

    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let stream = open_conversation_events(&pool, conversation.id).await;
    assert!(stream.is_ok(), "an existing conversation's events open");
}

/// SME-91 review: an idle release landing between a turn starting
/// (its generation recorded) and failing mustn't lose that failure.
#[test]
fn test_an_idle_release_keeps_a_starting_turns_generation() {
    let conversation_id = 9_100_000_192;
    let generation = new_turn_generation(conversation_id);
    release_idle_turn_state(conversation_id);
    keep_turn_error(conversation_id, generation, "it failed".to_string());
    assert_eq!(last_turn_error(conversation_id).as_deref(), Some("it failed"));
}

/// SME-91: a conversation's turn lock and stop counter go once its
/// turns are done, but not while someone still holds the lock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversations_turn_state_is_freed_after_its_turns(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000191)
        .await
        .expect("create conversation");
    start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    let turn_state_kept = holds_lock_and_stops;

    run_turn(&pool, conversation.id, hello()).await.expect("a turn");
    assert_eq!(turn_state_kept(conversation.id), (false, false), "freed after the turn");

    let held = conversation_lock(conversation.id);
    run_turn(&pool, conversation.id, hello()).await.expect("a turn");
    assert!(turn_state_kept(conversation.id).0, "a lock someone holds isn't replaced");
    drop(held);
}

/// Every tab can tell a turn is running, including one a background
/// notice started, and when it ends.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_turn_state_is_published_while_a_turn_runs(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000002)
        .await
        .expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let mut rx = events::subscribe(conversation.id);
    assert!(!turn_running(conversation.id));

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation.id, hello()).await }
    });
    let started = next_turn_state(&mut rx).await;
    assert_eq!(started, Some(true), "a turn starting should say so");
    assert!(turn_running(conversation.id));

    stop_turn_now(conversation.id);
    let _ = turn.await;
    let ended = next_turn_state(&mut rx).await;
    assert_eq!(ended, Some(false), "a turn ending (here, stopped) should say so");
    assert!(!turn_running(conversation.id));
}

#[test]
fn test_stop_turn_with_nothing_running_does_nothing() {
    stop_turn_now(9_000_000_012);
    let receiver = stop_receiver(9_000_000_012);
    assert!(!receiver.has_changed().expect("open"), "an earlier stop must not affect a new turn");
}

/// After a stop, a finished command doesn't wake the model; the user's
/// next message does, and its turn includes the command's notice.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_conversation_waits_for_the_user_before_waking(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000003)
        .await
        .expect("create conversation");
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;

    stop_turn_now(conversation.id);
    assert!(is_paused(conversation.id), "a stop pauses the conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-after-stop").await;
    wake_conversation(&pool, conversation.id)
        .await
        .expect("a paused wake does nothing, successfully");
    assert!(
        requests.lock().expect("log").is_empty(),
        "a paused conversation shouldn't call the model"
    );

    resume_turns(conversation.id);
    assert!(!is_paused(conversation.id));
    run_turn(&pool, conversation.id, hello())
        .await
        .expect("the user's next turn runs");
    let requests = requests.lock().expect("log");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["messages"].to_string().contains("cmd-after-stop"),
        "the next turn should include the command's notice"
    );
}

/// A notice delivered while nothing runs gets an answer from the model;
/// after the user stopped the conversation, it's only saved (SME-32
/// code review 6: the old save-then-wake never ran a turn).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_delivered_notice_is_answered_unless_stopped(pool: PgPool) {
    let _guard = lock_turn_tests();
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    deliver_notice(&pool, conversation.id, "Docker in your sandbox was restarted.".to_string()).await;
    {
        let requests = requests.lock().expect("the request log");
        assert_eq!(requests.len(), 1, "the model is asked about the notice");
        let last = requests[0]["messages"].as_array().expect("messages").last().expect("a message").to_string();
        assert!(last.contains("Docker in your sandbox was restarted."), "{last}");
    }

    let stopped = db::create_conversation(&pool).await.expect("create conversation");
    stop_turn_now(stopped.id);
    deliver_notice(&pool, stopped.id, "The user trusts it.".to_string()).await;
    assert_eq!(requests.lock().expect("the request log").len(), 1, "no model call after a stop");
    let saved = db::list_messages(&pool, stopped.id).await.expect("messages");
    assert!(saved.iter().any(|m| m.content.contains("The user trusts it.")), "saved for later");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_notice_queued_behind_a_stopped_turn_is_still_saved(pool: PgPool) {
    // SME-40 F4: a notice arriving during a turn (a command finishing,
    // say) queues as a turn of its own, behind the running one. Stop
    // ended both before the queued one had saved anything, so the notice
    // was lost and the model never learned the command finished.
    let conversation = db::create_conversation_with_id(&pool, 9100000040)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let running = lock.lock().await;

    let notice = "Command `make` in terminal 1 finished with exit code 0.";
    let queued = tokio::spawn({
        let pool = pool.clone();
        async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text { text: notice.to_string() }],
            };
            run_turn(&pool, conversation.id, message).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop_turn_now(conversation.id);
    // The running turn ends on the stop too, releasing the lock.
    drop(running);
    let result = queued.await.expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");

    let saved = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let messages = db::list_messages(&pool, conversation.id).await.expect("list");
            if messages.iter().any(|m| m.content.contains("finished with exit code 0")) {
                return messages;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(saved.is_ok(), "the queued notice was lost when the turn was stopped");
    resume_turns(conversation.id);
}

/// SME-91: one Stop is noted once, however many turns it ended, even
/// when the queued turns' own messages land after the first note.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_one_stop_is_noted_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000093)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let running = lock.lock().await;

    let queued = |text: &'static str| {
        let pool = pool.clone();
        tokio::spawn(async move {
            let message = anthropic::AnthropicMessage {
                role: "user".to_string(),
                content: vec![anthropic::ContentBlock::Text { text: text.to_string() }],
            };
            run_turn(&pool, conversation.id, message).await
        })
    };
    let first = queued("first notice");
    let second = queued("second notice");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop_turn_now(conversation.id);
    drop(running);
    for turn in [first, second] {
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("a stopped turn ends")
            .expect("join");
        assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    }

    let saved = db::list_messages(&pool, conversation.id).await.expect("list");
    let notes = saved.iter().filter(|m| m.content.contains(STOP_NOTICE)).count();
    assert_eq!(notes, 1, "one stop, one note: {saved:?}");
    assert!(saved.iter().any(|m| m.content.contains("first notice")));
    assert!(saved.iter().any(|m| m.content.contains("second notice")));
    resume_turns(conversation.id);
}

/// SME-91 (SME-51 code review 2): a turn that fails after the user
/// already sent the next message doesn't leave its error behind once
/// that next turn succeeds.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_older_turns_failure_isnt_kept_after_a_newer_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9100000094)
        .await
        .expect("create conversation");
    // The first request waits for `release`, then fails; later ones
    // answer.
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let gate = Arc::new(tokio::sync::Mutex::new(Some((reached_tx, release_rx))));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let gate = gate.clone();
            async move {
                if let Some((reached, release)) = gate.lock().await.take() {
                    let _ = reached.send(());
                    let _ = release.await;
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"type":"error","error":{"type":"invalid_request_error","message":"turn A failed"}}"#.to_string(),
                    )
                        .into_response();
                }
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    text_reply_body("B answered"),
                )
                    .into_response()
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(&pool, addr).await;

    start_turn(pool.clone(), conversation.id, "A".to_string()).await.expect("send A");
    tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx)
        .await
        .expect("A reaches the model")
        .expect("gate");
    start_turn(pool.clone(), conversation.id, "B".to_string()).await.expect("send B");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _ = release_tx.send(());

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let messages = db::list_messages(&pool, conversation.id).await.expect("list");
            if messages.iter().any(|m| m.content.contains("B answered")) && !turn_running(conversation.id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("B finishes");
    assert_eq!(last_turn_error(conversation.id), None, "A's error outlived B's success");
}

/// A notice waits for a running turn to end before it's saved, and
/// tells a watching tab once it is.
/// `notify` returns at once, even with a turn holding the lock (a caller
/// inside a turn can't wait for it), and its notices land in order once
/// the turn ends (SME-52).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_notify_returns_at_once_and_saves_in_order(pool: PgPool) {
    let conversation = db::create_conversation_with_id(&pool, 9100000052)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let turn = lock.lock().await;
    let notifying = notify(
        &pool,
        conversation.id,
        vec![Notice::Save("first".to_string()), Notice::Save("second".to_string())],
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        db::list_messages(&pool, conversation.id).await.expect("list").is_empty(),
        "a notice was saved while a turn held the lock"
    );
    drop(turn);
    tokio::time::timeout(std::time::Duration::from_secs(5), notifying)
        .await
        .expect("the notices are saved once the turn ends")
        .expect("join");
    let texts: Vec<String> = db::list_messages(&pool, conversation.id)
        .await
        .expect("list")
        .iter()
        .flat_map(|m| m.blocks().expect("blocks"))
        .filter_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_save_notice_between_turns_waits_for_the_turn_lock(pool: PgPool) {
    let conversation = db::create_conversation_with_id(&pool, 9100000004)
        .await
        .expect("create conversation");
    let lock = conversation_lock(conversation.id);
    let turn = lock.lock().await;
    let mut events = events::subscribe(conversation.id);

    let saving = tokio::spawn({
        let pool = pool.clone();
        async move {
            save_notice_between_turns(&pool, conversation.id, "pod stopped".to_string()).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        db::list_messages(&pool, conversation.id).await.expect("list").is_empty(),
        "the notice was saved while a turn held the lock"
    );

    drop(turn);
    let saved = saving.await.expect("join").expect("save the notice");
    assert_eq!(saved.role, "user");
    assert_eq!(
        saved.blocks().expect("blocks"),
        vec![anthropic::ContentBlock::Text { text: "pod stopped".to_string() }]
    );
    let appended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(events::ConversationEvent::MessagesAppended { messages }) => return messages,
                Ok(_) => continue,
                Err(e) => panic!("event channel: {e}"),
            }
        }
    })
    .await
    .expect("a MessagesAppended event");
    assert_eq!(appended.len(), 1);
}
