//! Per-conversation live event bus — the shared home for pushing updates
//! that happen with no `send_message` request in flight (a background
//! task's tick or completion), so neither `anthropic::tools` nor `api::chat`
//! has to depend on the other to publish or read these. `tools.rs` calls
//! `publish`; `chat.rs` calls both `publish` (after persisting a batch of
//! rows) and `subscribe` (to relay everything to a browser tab).

use serde::{Deserialize, Serialize};

use crate::anthropic::TokenUsage;
use crate::models::Message;

/// `TaskUpdate` is ephemeral UI telemetry, regenerable at any time from the
/// task registry (`anthropic::tools::snapshot_tasks`) — never persisted.
/// `MessagesAppended` carries no new data of its own; it's a live-delivery
/// notification for rows `db::create_message` already persisted. The three
/// `Sandbox*` variants are the same kind of ephemeral UI telemetry as
/// `TaskUpdate`, regenerable at any time from `api::chat::get_sandbox_state`
/// — see SME-10.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ConversationEvent {
    TaskUpdate {
        task_id: String,
        tool: String,
        status: String,
        /// Which stream `latest_output` came from — `"stdout"`/`"stderr"`,
        /// or `None` for a pure status transition (started/finished/...)
        /// that doesn't carry a line at all.
        stream: Option<String>,
        latest_output: Option<String>,
    },
    /// A struct variant, not `MessagesAppended(Vec<Message>)`: this enum
    /// is internally tagged, and serde can't write a tuple variant holding
    /// a list that way — every send failed, silently.
    MessagesAppended { messages: Vec<Message> },
    /// Published once on `create_pod` (already `Running` by the time it
    /// returns) and once on `terminate_pod`.
    SandboxPodUpdate {
        pod_id: i64,
        status: String,
        terminated: bool,
    },
    /// Published once on `create_terminal`, once on `terminate_terminal`,
    /// and once per terminal a crash-cleanup pass clears.
    SandboxTerminalUpdate {
        pod_id: i64,
        terminal_id: i64,
        status: String,
        terminated: bool,
    },
    /// Same shape/pattern as `TaskUpdate` — one variant covering "started",
    /// "one new output line", and "finished", distinguished by which
    /// optional fields are set. Deliberately doesn't carry `pod_id`: the
    /// frontend already knows a terminal's pod from `SandboxTerminalUpdate`,
    /// so a command update only ever needs to find an already-known
    /// terminal by `terminal_id`. `command` is only `Some` on the "started"
    /// event (published by `run_terminal_command_tool`, which has the
    /// command text in hand) — the output-line/finished events publish from
    /// `sandbox.rs`'s `handle_agent_message`, which only ever sees
    /// `command_id`, not the command it was for, and shouldn't pay for a DB
    /// lookup per output line just to repeat it.
    SandboxCommandUpdate {
        terminal_id: i64,
        command_id: String,
        command: Option<String>,
        status: String,
        exit_code: Option<i32>,
        stream: Option<String>,
        latest_output: Option<String>,
    },
    /// Published when `api::chat::wake_conversation` (fired when a terminal
    /// command finishes, to notify the model with no further tool call
    /// needed) fails to actually reach the model — e.g. `ANTHROPIC_API_KEY`
    /// unset, a transient Anthropic API error. The underlying notification
    /// text is still durably persisted regardless (`wake_conversation`
    /// drains and persists it *before* the API call that might fail) — this
    /// means "the model hasn't been prompted with it yet," not "it's lost."
    /// See SME-13.
    NotificationDeliveryFailed {
        detail: String,
    },
    /// Published after every real turn completes — ephemeral UI telemetry,
    /// same category as `TaskUpdate`/`Sandbox*Update`, regenerable at any
    /// time from `api::chat::get_context_usage`. See
    /// SME-18.
    ContextUsageUpdate {
        usage: TokenUsage,
        context_window: u32,
    },
    /// Published by `todowrite` on every call — carries the *complete*
    /// list (never a partial diff, matching `todowrite`'s own whole-list-
    /// replace semantics), so the frontend panel can just overwrite its
    /// signal wholesale rather than merging like `TaskUpdate` requires.
    /// Ephemeral UI telemetry, regenerable at any time from
    /// `db::get_conversation_todos`. See
    /// SME-20.
    TodoListUpdate {
        items: Vec<crate::anthropic::tools::TodoItem>,
    },
    /// Published by `open_browser_session`/`close_browser_session` — the
    /// live panel uses this to show/hide reactively rather than only
    /// checking on conversation (re)select, same reasoning
    /// `SandboxPodUpdate` already established for the sandbox panel.
    /// Ephemeral UI telemetry, regenerable at any time from
    /// `api::browsing::get_browsing_state`. See
    /// SME-22.
    BrowsingSessionUpdate {
        open: bool,
    },
    /// Published whenever a browsing session's page URL changes — the model
    /// navigating, a link click, a redirect, or an in-page change like
    /// `pushState` — so the live panel's address bar can follow along.
    /// Regenerable from `api::browsing::get_browsing_state`.
    BrowsingUrlUpdate {
        url: String,
    },
    /// An app-wide `AppEvent::PodsChanged` (a pod was created or went
    /// away, in any conversation), relayed on every conversation's stream
    /// so a chat tab doesn't need a second always-open connection for the
    /// sidebar's pod dots: browsers allow only 6 connections per host over
    /// HTTP/1.1, shared across tabs.
    PodsChanged {},
    /// Whether a model turn is running (or queued) in this conversation,
    /// published when that changes, so every tab watching it can offer a
    /// Stop button, including for turns it didn't start. Regenerable from
    /// `api::chat::get_turn_state`.
    TurnState {
        running: bool,
    },
    /// A model call is starting: whatever reply text a tab is showing as
    /// "streaming" is done with (it's saved, or it was a false start the
    /// retry replaces). Each model call gets its own streaming bubble.
    ReplyReset {},
    /// More reply text as the model streams it. Published for every
    /// turn, whoever started it, so every tab watching sees the reply
    /// arrive live. Regenerable mid-turn from
    /// `api::chat::get_reply_in_progress`.
    ReplyDelta {
        text: String,
    },
    /// A turn the user started by sending a message failed or was
    /// stopped (`message` is then `api::chat::TURN_STOPPED`). Turns
    /// started by a finished command or task report failure as
    /// `NotificationDeliveryFailed` instead.
    TurnError {
        message: String,
    },
}

/// Events that aren't about one conversation, for views that span them
/// all: the sidebar and the pods view. `subscribe_app_events` relays them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AppEvent {
    /// A pod was created or went away (stopped, crashed, or torn down with
    /// its conversation). Carries nothing: listeners refetch.
    PodsChanged,
}

#[cfg(feature = "server")]
mod server {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use tokio::sync::broadcast;

    use super::ConversationEvent;

    /// Bound on how many events a lagging subscriber can fall behind by
    /// before `broadcast` starts dropping its oldest ones. Replies stream as
    /// many small `ReplyDelta`s, so this leaves room for a tab that's a
    /// moment behind; one that falls further behind catches up at the
    /// reply's `MessagesAppended`. Each slot holds one event, cloned as
    /// each subscriber reads it; events are small.
    const CHANNEL_CAPACITY: usize = 1024;

    static BUSES: LazyLock<Mutex<HashMap<i64, broadcast::Sender<ConversationEvent>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn sender_for(conversation_id: i64) -> broadcast::Sender<ConversationEvent> {
        let mut buses = BUSES.lock().unwrap_or_else(|e| e.into_inner());
        buses
            .entry(conversation_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Publishes `event` to every current subscriber of `conversation_id`.
    /// Creates the underlying channel if this is the first event for that
    /// id. A no-op, cost-wise, if nobody's subscribed —
    /// `broadcast::Sender::send` on a channel with zero receivers just
    /// drops the value, which is why its `Result` is deliberately ignored.
    pub fn publish(conversation_id: i64, event: ConversationEvent) {
        let _ = sender_for(conversation_id).send(event);
    }

    /// Subscribes to `conversation_id`'s event stream from this point
    /// forward — `broadcast` has no replay, so events published before this
    /// call are never seen by this receiver. Creates the underlying channel
    /// if this is the first subscriber for that id (either side may be
    /// first).
    pub fn subscribe(conversation_id: i64) -> broadcast::Receiver<ConversationEvent> {
        sender_for(conversation_id).subscribe()
    }

    /// Drops `conversation_id`'s channel, for when the conversation is
    /// deleted. Its remaining subscribers see the channel close, which
    /// ends their event streams.
    pub fn forget(conversation_id: i64) {
        BUSES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&conversation_id);
    }

    /// The app-wide channel. One sender for the whole process; it never
    /// closes.
    static APP_BUS: LazyLock<broadcast::Sender<super::AppEvent>> =
        LazyLock::new(|| broadcast::channel(CHANNEL_CAPACITY).0);

    /// Publishes `event` to every app-wide subscriber.
    pub fn publish_app(event: super::AppEvent) {
        let _ = APP_BUS.send(event);
    }

    /// Subscribes to app-wide events from this point forward.
    pub fn subscribe_app() -> broadcast::Receiver<super::AppEvent> {
        APP_BUS.subscribe()
    }

    /// How many live app-wide subscriptions there are.
    #[cfg(test)]
    pub fn app_subscriber_count() -> usize {
        APP_BUS.receiver_count()
    }

    /// How many live subscriptions `conversation_id` has.
    #[cfg(test)]
    pub fn subscriber_count(conversation_id: i64) -> usize {
        sender_for(conversation_id).receiver_count()
    }

    #[cfg(test)]
    mod tests {
        use super::super::TokenUsage;
        use super::*;

        /// A reply streams as many small deltas; a tab that's a moment
        /// behind shouldn't lose any of them.
        #[tokio::test]
        async fn test_a_subscriber_keeps_a_burst_of_reply_deltas() {
            let conversation_id = 9_100_000_009;
            let mut rx = subscribe(conversation_id);
            for i in 0..500 {
                publish(conversation_id, ConversationEvent::ReplyDelta { text: i.to_string() });
            }
            for i in 0..500 {
                match rx.try_recv() {
                    Ok(ConversationEvent::ReplyDelta { text }) => assert_eq!(text, i.to_string()),
                    other => panic!("delta {i}: got {other:?}"),
                }
            }
        }

        #[tokio::test]
        async fn test_publish_app_reaches_app_subscribers() {
            let mut rx = subscribe_app();
            publish_app(super::super::AppEvent::PodsChanged);
            let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("an app event should arrive")
                .expect("the channel stays open");
            assert_eq!(received, super::super::AppEvent::PodsChanged);
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
            publish(
                1,
                ConversationEvent::TaskUpdate {
                    task_id: "t1".to_string(),
                    tool: "count".to_string(),
                    status: "running".to_string(),
                    stream: None,
                    latest_output: None,
                },
            );
            // No assertion beyond "doesn't panic" — there's nothing else to
            // observe when nobody's listening.
        }

        #[tokio::test]
        async fn test_subscribe_then_publish_delivers_event() {
            let mut rx = subscribe(2);
            let event = ConversationEvent::TaskUpdate {
                task_id: "t1".to_string(),
                tool: "count".to_string(),
                status: "running".to_string(),
                stream: Some("stdout".to_string()),
                latest_output: Some("count: 1/5".to_string()),
            };
            publish(2, event.clone());

            assert_eq!(rx.recv().await.expect("event should be delivered"), event);
        }

        #[tokio::test]
        async fn test_two_subscribers_both_receive_same_event() {
            let mut rx1 = subscribe(3);
            let mut rx2 = subscribe(3);
            let event = ConversationEvent::TaskUpdate {
                task_id: "t1".to_string(),
                tool: "add".to_string(),
                status: "finished".to_string(),
                stream: None,
                latest_output: None,
            };
            publish(3, event.clone());

            assert_eq!(rx1.recv().await.expect("rx1 should receive"), event);
            assert_eq!(rx2.recv().await.expect("rx2 should receive"), event);
        }

        #[tokio::test]
        async fn test_events_are_scoped_per_conversation() {
            let mut rx_a = subscribe(4);
            let rx_b_event = ConversationEvent::TaskUpdate {
                task_id: "t1".to_string(),
                tool: "add".to_string(),
                status: "finished".to_string(),
                stream: None,
                latest_output: None,
            };
            publish(5, rx_b_event);

            let a_event = ConversationEvent::TaskUpdate {
                task_id: "t2".to_string(),
                tool: "count".to_string(),
                status: "running".to_string(),
                stream: None,
                latest_output: None,
            };
            publish(4, a_event.clone());

            assert_eq!(
                rx_a.recv()
                    .await
                    .expect("conversation 4's subscriber should see its own event"),
                a_event
            );
        }

        /// Characterization test, not test-first: the three `Sandbox*`
        /// variants are a mechanical mirror of `TaskUpdate`'s already-tested
        /// shape on this same bus — see
        /// `docs/development-process.md`'s TDD exception for near-verbatim
        /// mirrors. Proves each variant round-trips (serializes, publishes,
        /// and is delivered back equal to what was sent), the same property
        /// the `TaskUpdate` tests above already establish for this bus.
        #[tokio::test]
        async fn test_sandbox_variants_round_trip_the_bus() {
            let mut rx = subscribe(6);

            let pod_event = ConversationEvent::SandboxPodUpdate {
                pod_id: 1,
                status: "Running".to_string(),
                terminated: false,
            };
            publish(6, pod_event.clone());
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
            publish(6, terminal_event.clone());
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
            };
            publish(6, command_event.clone());
            assert_eq!(
                rx.recv().await.expect("command event should be delivered"),
                command_event
            );

            let failure_event = ConversationEvent::NotificationDeliveryFailed {
                detail: "ANTHROPIC_API_KEY is not set on the server".to_string(),
            };
            publish(6, failure_event.clone());
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
            publish(6, context_usage_event.clone());
            assert_eq!(
                rx.recv()
                    .await
                    .expect("context-usage-update event should be delivered"),
                context_usage_event
            );
        }
    }
}

#[cfg(feature = "server")]
pub use server::{forget, publish, publish_app, subscribe, subscribe_app};
#[cfg(all(feature = "server", test))]
pub use server::{app_subscriber_count, subscriber_count};

#[cfg(test)]
mod wire_tests {
    use super::*;

    #[test]
    fn test_app_events_round_trip_through_json() {
        let event = AppEvent::PodsChanged;
        let json = serde_json::to_string(&event).expect("serialize");
        assert_eq!(json, r#"{"type":"PodsChanged"}"#);
        assert_eq!(serde_json::from_str::<AppEvent>(&json).expect("deserialize"), event);
    }

    fn one_of_each() -> Vec<ConversationEvent> {
        vec![
            ConversationEvent::TurnError { message: "boom".to_string() },
            ConversationEvent::ReplyReset {},
            ConversationEvent::ReplyDelta { text: "Hi".to_string() },
            ConversationEvent::TurnState { running: true },
            ConversationEvent::PodsChanged {},
            ConversationEvent::TaskUpdate {
                task_id: "t1".to_string(),
                tool: "count".to_string(),
                status: "running".to_string(),
                stream: Some("stdout".to_string()),
                latest_output: Some("1".to_string()),
            },
            ConversationEvent::MessagesAppended { messages: vec![Message {
                id: 7,
                conversation_id: 3,
                role: "assistant".to_string(),
                content: r#"[{"type":"text","text":"hi"}]"#.to_string(),
                created_at: chrono::DateTime::from_timestamp(1_790_000_000, 0)
                    .expect("valid timestamp")
                    .naive_utc(),
            }] },
            ConversationEvent::SandboxPodUpdate {
                pod_id: 1,
                status: "running".to_string(),
                terminated: false,
            },
            ConversationEvent::SandboxTerminalUpdate {
                pod_id: 1,
                terminal_id: 2,
                status: "connected".to_string(),
                terminated: false,
            },
            ConversationEvent::SandboxCommandUpdate {
                terminal_id: 2,
                command_id: "c1".to_string(),
                command: Some("ls".to_string()),
                status: "running".to_string(),
                exit_code: None,
                stream: None,
                latest_output: None,
            },
            ConversationEvent::NotificationDeliveryFailed {
                detail: "boom".to_string(),
            },
            ConversationEvent::ContextUsageUpdate {
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_creation_input_tokens: 3,
                    cache_read_input_tokens: 4,
                },
                context_window: 200_000,
            },
            ConversationEvent::TodoListUpdate {
                items: vec![crate::anthropic::tools::TodoItem {
                    content: "ship it".to_string(),
                    status: crate::anthropic::tools::TodoStatus::Pending,
                }],
            },
            ConversationEvent::BrowsingSessionUpdate { open: true },
            ConversationEvent::BrowsingUrlUpdate {
                url: "https://example.com/".to_string(),
            },
        ]
    }

    /// Every event goes to the browser as JSON, and a variant serde can't
    /// write is silently dropped on the way — so every variant must
    /// survive a round trip.
    #[test]
    fn test_every_event_round_trips_through_json() {
        for event in one_of_each() {
            let json = serde_json::to_string(&event)
                .unwrap_or_else(|e| panic!("{event:?} doesn't serialize: {e}"));
            let back: ConversationEvent = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("{json} doesn't deserialize: {e}"));
            assert_eq!(back, event);
        }
    }
}
