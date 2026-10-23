use super::*;

#[test]
fn test_forget_conversation_lock_drops_it() {
    let conversation_id = 987_654_011;
    let first = conversation_lock(conversation_id);
    let _ = stop_receiver(conversation_id);
    forget_conversation_lock(conversation_id);
    assert!(
        !holds_lock_and_stops(conversation_id).1,
        "the deleted conversation's stop signal is still registered"
    );
    let second = conversation_lock(conversation_id);
    assert!(
        !Arc::ptr_eq(&first, &second),
        "the deleted conversation's lock is still registered"
    );
    forget_conversation_lock(conversation_id);
}

/// A tab watching a conversation also hears app-wide pod changes, on
/// the same stream.
#[tokio::test]
async fn test_a_conversation_stream_relays_pods_changed() {
    let stream = conversation_event_stream(9_000_000_011);
    futures_util::pin_mut!(stream);
    // Another test's app event first, as one running alongside can
    // publish.
    events::publish_app(events::AppEvent::TurnsChanged);
    events::publish_app(events::AppEvent::PodsChanged);
    let event = next_skipping_other_app_relays(&mut stream, |e| {
        matches!(e, events::ConversationEvent::PodsChanged {})
    })
    .await;
    assert!(
        matches!(event, Some(Ok(events::ConversationEvent::PodsChanged {}))),
        "got {event:?}"
    );
}

/// Whether `event` is one a conversation's stream relays from the
/// app-wide channel, which every test running alongside can publish to.
fn is_app_relay(event: &events::ConversationEvent) -> bool {
    matches!(
        event,
        events::ConversationEvent::PodsChanged {}
            | events::ConversationEvent::TurnsChanged {}
            | events::ConversationEvent::QuestionsChanged {}
            | events::ConversationEvent::ProvidersChanged {}
            | events::ConversationEvent::ConversationsChanged {}
    )
}

/// The stream's next item that is `wanted`, or isn't a relayed app
/// event at all: another test's app events aren't this test's to judge
/// (SME-135). Gives up after a second.
async fn next_skipping_other_app_relays(
    stream: &mut (impl futures_util::Stream<Item = Result<events::ConversationEvent, axum::BoxError>> + Unpin),
    wanted: impl Fn(&events::ConversationEvent) -> bool,
) -> Option<Result<events::ConversationEvent, axum::BoxError>> {
    use futures_util::StreamExt;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match stream.next().await {
                Some(Ok(event)) if is_app_relay(&event) && !wanted(&event) => continue,
                other => return other,
            }
        }
    })
    .await
    .expect("the wanted event should arrive on the conversation stream")
}

/// SME-269: the message that lands is also the one that may name the
/// conversation and move it to the top of the list, so a `ConversationsChanged`
/// rides along with every `MessagesAppended`. Read the way a tab does, through
/// `open_conversation_events`: it must reach the stream, not only the app-wide
/// channel.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_saved_message_also_announces_a_list_change(pool: PgPool) {
    use futures_util::StreamExt;
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("conversation").id;
    let stream = open_conversation_events(&pool, conversation_id).await.expect("the conversation's stream");
    futures_util::pin_mut!(stream);
    let saved = db::create_message(
        &pool,
        conversation_id,
        "user",
        &[anthropic::ContentBlock::Text { text: "the first message".to_string() }],
    )
    .await
    .expect("message");
    let mut persisted = Vec::new();
    record_saved(conversation_id, &mut persisted, saved.clone());

    let mut saw_appended = false;
    let mut saw_list_change = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !(saw_appended && saw_list_change) {
        assert!(
            std::time::Instant::now() < deadline,
            "messages appended: {saw_appended}, list change: {saw_list_change}"
        );
        match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("the conversation's stream to answer")
        {
            Some(Ok(events::ConversationEvent::MessagesAppended { messages })) => {
                assert_eq!(messages, vec![saved.clone()]);
                saw_appended = true;
            }
            Some(Ok(events::ConversationEvent::ConversationsChanged {})) => saw_list_change = true,
            // Another test's app event relayed onto this stream (SME-135).
            Some(Ok(event)) if is_app_relay(&event) => continue,
            other => panic!("the conversation's stream gave {other:?}"),
        }
    }
    events::forget(conversation_id);
}

/// SME-51 B3: each delta says where it starts, measured against the
/// same text `get_reply_in_progress` returns.
#[tokio::test]
async fn test_reply_deltas_carry_their_offset_in_the_reply_so_far() {
    let conversation_id = 9_000_000_052;
    let mut rx = events::subscribe(conversation_id);
    relay_reply_delta(conversation_id, "héllo ");
    relay_reply_delta(conversation_id, "world");
    let mut offsets = Vec::new();
    while let Ok(events::ConversationEvent::ReplyDelta { offset, .. }) = rx.try_recv() {
        offsets.push(offset);
    }
    assert_eq!(offsets, vec![0, "héllo ".len()]);
    assert_eq!(reply_in_progress(conversation_id).as_deref(), Some("héllo world"));
    clear_reply_in_progress(conversation_id);
    events::forget(conversation_id);
}

/// SME-51 B3: a tab that falls behind loses events it can't get back
/// (a saved message, the turn ending). Its stream ends instead, so the
/// tab reconnects and pulls the current state again.
#[tokio::test]
async fn test_a_stream_that_falls_behind_ends_instead_of_skipping() {
    use futures_util::StreamExt;
    let conversation_id = 9_000_000_051;
    let stream = conversation_event_stream(conversation_id);
    futures_util::pin_mut!(stream);
    for i in 0..2_000 {
        events::publish(
            conversation_id,
            events::ConversationEvent::ReplyDelta { text: format!("{i} "), offset: 0 },
        );
    }
    // An app event another test can publish meanwhile (the providers
    // tests do), which the stream relays.
    events::publish_app(events::AppEvent::ProvidersChanged);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match stream.next().await {
                None => return true,
                // App-wide events, this test's own and those of tests
                // running alongside: all four kinds the stream relays.
                Some(Ok(event)) if is_app_relay(&event) => continue,
                Some(_) => return false,
            }
        }
    })
    .await
    .expect("the stream should answer");
    assert!(ended, "a lagging stream kept going with events missing");
    events::forget(conversation_id);
}

/// SME-41 D9: a turn starting or ending is announced app-wide, and
/// the conversation is listed as busy while it runs, so the sidebar
/// can mark it in every tab.
#[tokio::test]
async fn test_a_running_turn_marks_its_conversation_busy() {
    let conversation_id = 9_000_000_041;
    let mut app = events::subscribe_app();
    let next_turns_changed = |app: &mut tokio::sync::broadcast::Receiver<events::AppEvent>| {
        let mut app = app.resubscribe();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if let Ok(events::AppEvent::TurnsChanged) = app.recv().await {
                        return;
                    }
                }
            })
            .await
            .is_ok()
        }
    };
    let started = next_turns_changed(&mut app);
    let turn = TurnInFlight::start(conversation_id);
    assert!(started.await, "starting a turn should announce it");
    assert!(busy_conversations().contains(&conversation_id));

    let ended = next_turns_changed(&mut app);
    drop(turn);
    assert!(ended.await, "ending a turn should announce it");
    assert!(!busy_conversations().contains(&conversation_id));
}

/// SME-160: the working timer's age comes from when the busy spell
/// started, on the server's monotonic clock: a turn queuing behind a
/// running one doesn't move the start, the age keeps climbing while any
/// turn runs (not from whenever a tab connected), it's gone once the last
/// turn ends, and the next spell starts its own count. No assertion here
/// bounds the age in seconds from above, since a loaded runner could
/// always stall long enough to trip one: each age is compared with this
/// test's own clock over the same span, or with an earlier age.
/// `TurnState` carries it on the wire.
#[tokio::test]
async fn test_turn_started_elapsed_counts_from_the_busy_spell_start() {
    let conversation_id = 9_160_000_016;
    let mut rx = events::subscribe(conversation_id);
    assert_eq!(turn_started_elapsed(conversation_id), None, "idle has no age");

    // The record is made when the spell is, so the age it reports can
    // never be ahead of a clock this test started before the spell.
    let spell = std::time::Instant::now();
    let running = TurnInFlight::start(conversation_id);
    let event = rx.recv().await.expect("a starting turn announces itself");
    assert!(
        matches!(event, events::ConversationEvent::TurnState { running: true, elapsed: 0 }),
        "got {event:?}"
    );
    let age = turn_started_elapsed(conversation_id).expect("a running turn has an age");
    assert!(
        age <= spell.elapsed(),
        "the age is ahead of the wall clock it counts from: {age:?} against {:?}",
        spell.elapsed()
    );

    // The clock moves with the wall clock: after a real second it has
    // counted it, from the spell's start rather than from now.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let before = turn_started_elapsed(conversation_id).expect("the turn is still running");
    assert!(
        before >= std::time::Duration::from_secs(1),
        "the clock didn't move: {before:?}"
    );

    // A turn queuing behind the running one doesn't restart the spell,
    // and ending the running one while the queued one remains doesn't
    // either — the busy spell is still one spell. A start that moved with
    // the queued turn would set the age back to about 0, so it is read as
    // never smaller than the second already counted.
    let queued = TurnInFlight::start(conversation_id);
    drop(running);
    let after = turn_started_elapsed(conversation_id).expect("the queued turn keeps the spell running");
    assert!(
        after >= before,
        "the start moved with the queued turn: {before:?} then {after:?}"
    );
    assert!(rx.try_recv().is_err(), "a queued turn or a partial end publishes nothing");

    // The spell ends with the last turn...
    drop(queued);
    assert_eq!(turn_started_elapsed(conversation_id), None, "idle again has no age");
    let event = rx.recv().await.expect("the last turn ending announces itself");
    assert!(
        matches!(event, events::ConversationEvent::TurnState { running: false, elapsed: 0 }),
        "got {event:?}"
    );

    // ...and the next spell keeps nothing of the old one. An age inherited
    // from the first spell would be seconds ahead of this new clock.
    let next_spell = std::time::Instant::now();
    let fresh = TurnInFlight::start(conversation_id);
    let event = rx.recv().await.expect("the next spell announces itself");
    assert!(
        matches!(event, events::ConversationEvent::TurnState { running: true, elapsed: 0 }),
        "got {event:?}"
    );
    let age = turn_started_elapsed(conversation_id).expect("the next turn has its own age");
    assert!(
        age <= next_spell.elapsed(),
        "the new spell kept the old one's clock: {age:?} against {:?}",
        next_spell.elapsed()
    );
    drop(fresh);
    events::forget(conversation_id);
}

#[tokio::test]
async fn test_a_conversation_stream_relays_turns_changed() {
    let stream = conversation_event_stream(9_000_000_042);
    futures_util::pin_mut!(stream);
    // Another test's app event first, as one running alongside can
    // publish (a sandbox test's pod going away, say).
    events::publish_app(events::AppEvent::PodsChanged);
    events::publish_app(events::AppEvent::TurnsChanged);
    let event = next_skipping_other_app_relays(&mut stream, |e| {
        matches!(e, events::ConversationEvent::TurnsChanged {})
    })
    .await;
    assert!(
        matches!(event, Some(Ok(events::ConversationEvent::TurnsChanged {}))),
        "got {event:?}"
    );
}

/// A subscription belongs to its connection: once the response is
/// dropped (the tab closed or reloaded), nothing should still be
/// listening on the conversation's channel.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_dropped_event_subscription_stops_listening(pool: PgPool) {
    // An id no other test's conversation has: subscriptions are counted
    // per id across the process, and every test database counts from 1
    // (it failed twice in SME-94's gates, with turn tests running
    // alongside once nothing held their lock for minutes).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    // What `subscribe_conversation_events` answers, on this test's pool.
    let subscription = ServerEvents::from_stream(
        open_conversation_events(&pool, conversation_id).await.expect("subscribe"),
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(events::subscriber_count(conversation_id), 1);
    drop(subscription);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        events::subscriber_count(conversation_id),
        0,
        "a dropped subscription is still listening"
    );
}
