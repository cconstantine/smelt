use super::super::TokenUsage;
use super::*;

/// SME-135: a test listening on an id every test database hands
/// out (conversation 1, say) would hear, or count, other tests'
/// events depending on what runs alongside it; it's refused.
#[test]
#[should_panic(expected = "tests subscribe only on a conversation id of their own")]
fn test_subscribing_on_an_id_test_databases_share_is_refused() {
    let _subscription = subscribe(1);
}

/// The ids tests do pick, by hand or from
/// `start_ids_clear_of_other_runs`, are all above the shared range,
/// which ends where it says. The boundary is checked without a
/// channel: a literal id outside a ticket's block is one a later
/// test could pick too.
#[test]
fn test_an_id_above_the_shared_range_can_be_subscribed() {
    refuse_an_id_test_databases_share(TEST_SHARED_IDS_END);
    let conversation_id = 9_135_000_100;
    let subscription = subscribe(conversation_id);
    assert_eq!(subscriber_count(conversation_id), 1);
    drop(subscription);
    assert!(!has_channel(conversation_id));
}

/// SME-91: a conversation's channel (about 150 KB) goes with its
/// last subscriber, rather than staying for good.
#[test]
fn test_a_channel_is_freed_with_its_last_subscriber() {
    let conversation_id = 9_100_000_097;
    let first = subscribe(conversation_id);
    let second = subscribe(conversation_id);
    drop(first);
    assert!(has_channel(conversation_id), "one subscriber is left");
    drop(second);
    assert!(!has_channel(conversation_id), "the last subscriber took the channel with it");
}

/// SME-91 review: two subscribers leaving at the same moment (two
/// tabs closing) still free the channel: each counted the other's
/// receiver, which went only after its own count.
#[test]
fn test_two_subscribers_leaving_at_once_free_the_channel() {
    for round in 0..2000 {
        let conversation_id = 9_100_010_000 + round;
        let first = subscribe(conversation_id);
        let second = subscribe(conversation_id);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let other = std::thread::spawn({
            let barrier = barrier.clone();
            move || {
                barrier.wait();
                drop(first);
            }
        });
        barrier.wait();
        drop(second);
        other.join().expect("join");
        assert!(!has_channel(conversation_id), "round {round}: the channel was left behind");
    }
}

/// SME-91: publishing with nobody listening (a deleted
/// conversation's late events, say) makes no channel.
#[test]
fn test_a_publish_with_no_subscriber_makes_no_channel() {
    let conversation_id = 9_100_000_098;
    publish(conversation_id, ConversationEvent::TurnState { running: false, elapsed: 0 });
    assert!(!has_channel(conversation_id));
}

/// A reply streams as many small deltas; a tab that's a moment
/// behind shouldn't lose any of them.
#[tokio::test]
async fn test_a_subscriber_keeps_a_burst_of_reply_deltas() {
    let conversation_id = 9_100_000_009;
    let mut rx = subscribe(conversation_id);
    for i in 0..500 {
        publish(conversation_id, ConversationEvent::ReplyDelta { text: i.to_string(), offset: 0 });
    }
    for i in 0..500 {
        match rx.try_recv() {
            Ok(ConversationEvent::ReplyDelta { text, .. }) => assert_eq!(text, i.to_string()),
            other => panic!("delta {i}: got {other:?}"),
        }
    }
}

#[tokio::test]
async fn test_publish_app_reaches_app_subscribers() {
    let mut rx = subscribe_app();
    // Another test's app event first, as one running alongside
    // can publish.
    publish_app(super::super::AppEvent::TurnsChanged);
    publish_app(super::super::AppEvent::PodsChanged);
    // The app-wide channel is shared with every test running
    // alongside: wait for this one, skipping theirs (SME-135).
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let received = rx.recv().await.expect("the channel stays open");
            if received == super::super::AppEvent::PodsChanged {
                return;
            }
        }
    })
    .await
    .expect("PodsChanged should arrive");
}

#[tokio::test]
async fn test_forget_closes_the_conversations_channel() {
    let conversation_id = 987_654_012;
    let mut rx = subscribe(conversation_id);
    forget(conversation_id);
    let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("the channel should close, not stay open");
    assert!(
        matches!(received, Err(broadcast::error::RecvError::Closed)),
        "expected the channel to close, got {received:?}"
    );
}

#[tokio::test]
async fn test_publish_with_no_subscribers_is_a_noop() {
    // An id of its own: an event on a shared "conversation 1"
    // would reach every other test listening on theirs (SME-135).
    publish(
        9_135_000_001,
        ConversationEvent::SandboxCommandUpdate {
            terminal_id: 1,
            command_id: "t1".to_string(),
            command: None,
            status: "running".to_string(),
            exit_code: None,
            stream: None,
            latest_output: None,
            position: None,
        },
    );
    // No assertion beyond "doesn't panic" — there's nothing else to
    // observe when nobody's listening.
}

#[tokio::test]
async fn test_subscribe_then_publish_delivers_event() {
    let mut rx = subscribe(9_135_000_002);
    let event = ConversationEvent::SandboxCommandUpdate {
        terminal_id: 1,
        command_id: "t1".to_string(),
        command: None,
        status: "running".to_string(),
        exit_code: None,
        stream: Some("stdout".to_string()),
        latest_output: Some("count: 1/5".to_string()),
        position: None,
    };
    publish(9_135_000_002, event.clone());

    assert_eq!(rx.recv().await.expect("event should be delivered"), event);
}

#[tokio::test]
async fn test_two_subscribers_both_receive_same_event() {
    let mut rx1 = subscribe(9_135_000_003);
    let mut rx2 = subscribe(9_135_000_003);
    let event = ConversationEvent::SandboxCommandUpdate {
        terminal_id: 1,
        command_id: "t1".to_string(),
        command: None,
        status: "finished".to_string(),
        exit_code: None,
        stream: None,
        latest_output: None,
        position: None,
    };
    publish(9_135_000_003, event.clone());

    assert_eq!(rx1.recv().await.expect("rx1 should receive"), event);
    assert_eq!(rx2.recv().await.expect("rx2 should receive"), event);
}

#[tokio::test]
async fn test_events_are_scoped_per_conversation() {
    let mut rx_a = subscribe(9_135_000_004);
    let rx_b_event = ConversationEvent::SandboxCommandUpdate {
        terminal_id: 1,
        command_id: "t1".to_string(),
        command: None,
        status: "finished".to_string(),
        exit_code: None,
        stream: None,
        latest_output: None,
        position: None,
    };
    publish(9_135_000_005, rx_b_event);

    let a_event = ConversationEvent::SandboxCommandUpdate {
        terminal_id: 1,
        command_id: "t2".to_string(),
        command: None,
        status: "running".to_string(),
        exit_code: None,
        stream: None,
        latest_output: None,
        position: None,
    };
    publish(9_135_000_004, a_event.clone());

    assert_eq!(
        rx_a.recv()
            .await
            .expect("conversation 9_135_000_004's subscriber should see its own event"),
        a_event
    );
}

/// Characterization test, not test-first: the `Sandbox*` variants
/// are a mechanical mirror of each other on this same bus — see
/// `docs/process/tdd.md`'s TDD exception for near-verbatim
/// mirrors. Proves each variant round-trips (serializes, publishes,
/// and is delivered back equal to what was sent).
#[tokio::test]
async fn test_sandbox_variants_round_trip_the_bus() {
    let mut rx = subscribe(9_135_000_006);

    let pod_event = ConversationEvent::SandboxPodUpdate {
        pod_id: 1,
        status: "Running".to_string(),
        terminated: false,
    };
    publish(9_135_000_006, pod_event.clone());
    assert_eq!(
        rx.recv().await.expect("pod event should be delivered"),
        pod_event
    );

    let terminal_event = ConversationEvent::SandboxTerminalUpdate {
        pod_id: 1,
        terminal_id: 2,
        status: "connected".to_string(),
        terminated: false,
    };
    publish(9_135_000_006, terminal_event.clone());
    assert_eq!(
        rx.recv().await.expect("terminal event should be delivered"),
        terminal_event
    );

    let command_event = ConversationEvent::SandboxCommandUpdate {
        terminal_id: 2,
        command_id: "cmd-1".to_string(),
        command: Some("echo hi".to_string()),
        status: "running".to_string(),
        exit_code: None,
        stream: Some("stdout".to_string()),
        latest_output: Some("hi".to_string()),
        position: None,
    };
    publish(9_135_000_006, command_event.clone());
    assert_eq!(
        rx.recv().await.expect("command event should be delivered"),
        command_event
    );

    let failure_event = ConversationEvent::NotificationDeliveryFailed {
        detail: "No model is chosen for this conversation.".to_string(),
    };
    publish(9_135_000_006, failure_event.clone());
    assert_eq!(
        rx.recv()
            .await
            .expect("notification-delivery-failed event should be delivered"),
        failure_event
    );

    let context_usage_event = ConversationEvent::ContextUsageUpdate {
        usage: TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
        context_window: 200_000,
    };
    publish(9_135_000_006, context_usage_event.clone());
    assert_eq!(
        rx.recv()
            .await
            .expect("context-usage-update event should be delivered"),
        context_usage_event
    );
}
