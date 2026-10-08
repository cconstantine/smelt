//! Per-conversation live event bus — the shared home for pushing updates
//! that happen with no `send_message` request in flight (a command's
//! output or completion), so neither `anthropic::tools` nor `api::chat`
//! has to depend on the other to publish or read these. The tools call
//! `publish`; `chat.rs` calls both `publish` (after persisting a batch of
//! rows) and `subscribe` (to relay everything to a browser tab).

use serde::{Deserialize, Serialize};

use crate::anthropic::TokenUsage;
use crate::models::Message;

/// A port in a sandbox pod the user can open in their own browser, and the
/// preview address that reaches it (`preview::PreviewTemplate::url_for`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxPreview {
    pub port: u16,
    /// A Docker container's address in the pod, or `None` for the pod's
    /// own localhost (SME-33).
    #[serde(default)]
    pub host: Option<String>,
    pub url: String,
}

impl SandboxPreview {
    /// What the preview reaches, as the panel names it: `port 3000`, or a
    /// container's `172.21.0.2:3000`.
    pub fn label(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}:{}", self.port),
            None => format!("port {}", self.port),
        }
    }
}

/// `MessagesAppended` carries no new data of its own; it's a live-delivery
/// notification for rows `db::create_message` already persisted. The
/// `Sandbox*` variants are ephemeral UI telemetry, never persisted and
/// regenerable at any time from `api::sandbox::get_sandbox_state` — see
/// SME-10.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ConversationEvent {
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
    /// Published when the model shares a preview (`sandbox_preview_url`),
    /// carrying every preview that pod has so far — the panel replaces its
    /// list with this one. See SME-42.
    SandboxPreviewUpdate {
        pod_id: i64,
        previews: Vec<SandboxPreview>,
    },
    /// Published once on `create_terminal`, once on `terminate_terminal`,
    /// and once per terminal a crash-cleanup pass clears.
    SandboxTerminalUpdate {
        pod_id: i64,
        terminal_id: i64,
        status: String,
        terminated: bool,
    },
    /// One variant covering "started", "one new output line", and
    /// "finished", distinguished by which optional fields are set. `stream`
    /// is `"stdout"`/`"stderr"`, or `None` for a pure status transition. Deliberately doesn't carry `pod_id`: the
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
        /// The agent's `seq` for `latest_output`, one counter per command
        /// across both streams. A tab that reconnects skips a line its
        /// snapshot already has (SME-51 B3). `None` when there's no line.
        position: Option<i64>,
    },
    /// Published when a `turn::notify` wake or delivery (fired when a terminal
    /// command finishes, to notify the model with no further tool call
    /// needed) fails to actually reach the model — e.g. no model provider
    /// set up, a transient Anthropic API error. The underlying notification
    /// text is still durably persisted regardless (`wake_conversation`
    /// drains and persists it *before* the API call that might fail) — this
    /// means "the model hasn't been prompted with it yet," not "it's lost."
    /// See SME-13.
    NotificationDeliveryFailed {
        detail: String,
    },
    /// Published after every real turn completes — ephemeral UI telemetry,
    /// same category as `Sandbox*Update`, regenerable at any
    /// time from `api::chat::get_context_usage`. See
    /// SME-18.
    ContextUsageUpdate {
        usage: TokenUsage,
        context_window: u32,
    },
    /// Published by `todowrite` on every call — carries the *complete*
    /// list (never a partial diff, matching `todowrite`'s own whole-list-
    /// replace semantics), so the frontend panel can just overwrite its
    /// signal wholesale rather than merging line by line.
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
    /// The conversation's repos (SME-32), all of them, published whenever
    /// one is added, starts cloning, or finishes. Regenerable from
    /// `api::git::list_conversation_repos`.
    ReposUpdate {
        repos: Vec<crate::git::RepoSummary>,
    },
    /// An app-wide `AppEvent::PodsChanged` (a pod was created or went
    /// away, in any conversation), relayed on every conversation's stream
    /// so a chat tab doesn't need a second always-open connection for the
    /// sidebar's pod dots: browsers allow only 6 connections per host over
    /// HTTP/1.1, shared across tabs.
    PodsChanged {},
    /// An app-wide `AppEvent::TurnsChanged` (a turn started or ended in
    /// some conversation), relayed like `PodsChanged` so the sidebar can
    /// mark which conversations are busy (SME-41 D9).
    TurnsChanged {},
    /// The question this conversation waits on (SME-34): the model asked
    /// one (`Some`), or it was answered or dismissed (`None`). Regenerable
    /// from `api::questions::get_pending_question`.
    QuestionUpdate {
        question: Option<crate::questions::PendingQuestion>,
    },
    /// An app-wide `AppEvent::QuestionsChanged`, relayed like `TurnsChanged`
    /// so the sidebar can mark conversations waiting on an answer.
    QuestionsChanged {},
    /// The conversation's provider or model changed (the user chose
    /// another, or a turn took the default). Carries nothing: the tab
    /// refetches `api::chat::get_conversation_model` (SME-72).
    ModelChanged {},
    /// An app-wide `AppEvent::ProvidersChanged` (a provider or the default
    /// model was added, edited or removed), relayed like `PodsChanged` so
    /// the model picker and the no-provider notice stay current (SME-72).
    ProvidersChanged {},
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
        /// The reply's length in bytes before `text`, so a tab that
        /// fetched the reply so far skips what it already has (SME-51 B3).
        offset: usize,
    },
    /// A turn the user started by sending a message failed or was
    /// stopped (`message` is then `api::chat::TURN_STOPPED`). Turns
    /// started by a finished command or task report failure as
    /// `NotificationDeliveryFailed` instead.
    TurnError {
        message: String,
    },
    /// A type the browser tier's web bundle doesn't have (`dx build` leaves
    /// this feature off), standing in for one a newer server adds.
    #[cfg(feature = "browser-test")]
    BrowserTestAddedLater {},
    /// An event type this build doesn't know: one added on a newer server
    /// than the page's bundle. Never published; a tab skips it and offers
    /// a reload (SME-43).
    #[serde(other)]
    Unknown,
}

/// Events that aren't about one conversation, for views that span them
/// all: the sidebar and the pods view. `subscribe_app_events` relays them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AppEvent {
    /// A pod was created or went away (stopped, crashed, or torn down with
    /// its conversation). Carries nothing: listeners refetch.
    PodsChanged,
    /// A model turn started or ended in some conversation. Carries
    /// nothing: listeners refetch `api::chat::get_busy_conversations`.
    TurnsChanged,
    /// A conversation started or stopped waiting on an answer to the
    /// model's question (SME-34). Carries nothing: listeners refetch
    /// `api::questions::get_waiting_conversations`.
    QuestionsChanged,
    /// A model provider or the default model was added, edited or
    /// removed. Carries nothing: listeners refetch (SME-72).
    ProvidersChanged,
    /// As `ConversationEvent::Unknown`.
    #[serde(other)]
    Unknown,
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

    /// Publishes `event` to every current subscriber of `conversation_id`.
    /// With nobody subscribed there's no channel, and the event goes
    /// nowhere, as it would have anyway: a channel exists only while a tab
    /// (or a test) listens, so a deleted conversation's late events make
    /// none (SME-91). Sent under the map's lock, so it can't reach a
    /// channel being replaced.
    pub fn publish(conversation_id: i64, event: ConversationEvent) {
        let buses = BUSES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sender) = buses.get(&conversation_id) {
            let _ = sender.send(event);
        }
    }

    /// Subscribes to `conversation_id`'s event stream from this point
    /// forward — `broadcast` has no replay, so events published before this
    /// call are never seen by this receiver. Creates the channel if this is
    /// the first subscriber; the last one to go takes it away again
    /// (`Subscription`'s drop).
    pub fn subscribe(conversation_id: i64) -> Subscription {
        let mut buses = BUSES.lock().unwrap_or_else(|e| e.into_inner());
        let sender = buses
            .entry(conversation_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0);
        Subscription {
            conversation_id,
            channel: sender.downgrade(),
            receiver: sender.subscribe(),
        }
    }

    /// A subscription to one conversation's events: a `broadcast::Receiver`
    /// (it derefs to one) that frees the conversation's channel when it's
    /// the last to go.
    pub struct Subscription {
        conversation_id: i64,
        /// Weak, so a deleted conversation's channel still closes
        /// (`forget`) while this lives.
        channel: broadcast::WeakSender<ConversationEvent>,
        receiver: broadcast::Receiver<ConversationEvent>,
    }

    impl std::ops::Deref for Subscription {
        type Target = broadcast::Receiver<ConversationEvent>;
        fn deref(&self) -> &Self::Target {
            &self.receiver
        }
    }

    impl std::ops::DerefMut for Subscription {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.receiver
        }
    }

    impl Drop for Subscription {
        fn drop(&mut self) {
            // Under the map's lock, which subscribing also takes, so no one
            // can join the channel between the count and the removal. This
            // receiver is let go first, inside the lock: counted while it
            // lived, two subscribers leaving at once each saw the other and
            // left the channel behind (SME-91 review). A spare receiver of
            // a tiny channel takes its place for the rest of the drop.
            let mut buses = BUSES.lock().unwrap_or_else(|e| e.into_inner());
            drop(std::mem::replace(&mut self.receiver, broadcast::channel(1).1));
            let last_on_this_channel = match (buses.get(&self.conversation_id), self.channel.upgrade()) {
                (Some(current), Some(own)) => current.same_channel(&own) && current.receiver_count() == 0,
                _ => false,
            };
            if last_on_this_channel {
                buses.remove(&self.conversation_id);
            }
        }
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

    /// How many live subscriptions `conversation_id` has.
    #[cfg(test)]
    pub fn subscriber_count(conversation_id: i64) -> usize {
        BUSES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&conversation_id)
            .map_or(0, |sender| sender.receiver_count())
    }

    /// Whether `conversation_id` has a channel at all.
    #[cfg(test)]
    pub fn has_channel(conversation_id: i64) -> bool {
        BUSES.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&conversation_id)
    }

    #[cfg(test)]
    mod tests {
        use super::super::TokenUsage;
        use super::*;

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
            publish(conversation_id, ConversationEvent::TurnState { running: false });
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
            let mut rx = subscribe(2);
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
            publish(2, event.clone());

            assert_eq!(rx.recv().await.expect("event should be delivered"), event);
        }

        #[tokio::test]
        async fn test_two_subscribers_both_receive_same_event() {
            let mut rx1 = subscribe(3);
            let mut rx2 = subscribe(3);
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
            publish(3, event.clone());

            assert_eq!(rx1.recv().await.expect("rx1 should receive"), event);
            assert_eq!(rx2.recv().await.expect("rx2 should receive"), event);
        }

        #[tokio::test]
        async fn test_events_are_scoped_per_conversation() {
            let mut rx_a = subscribe(4);
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
            publish(5, rx_b_event);

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
            publish(4, a_event.clone());

            assert_eq!(
                rx_a.recv()
                    .await
                    .expect("conversation 4's subscriber should see its own event"),
                a_event
            );
        }

        /// Characterization test, not test-first: the `Sandbox*` variants
        /// are a mechanical mirror of each other on this same bus — see
        /// `docs/development-process.md`'s TDD exception for near-verbatim
        /// mirrors. Proves each variant round-trips (serializes, publishes,
        /// and is delivered back equal to what was sent).
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
                position: None,
            };
            publish(6, command_event.clone());
            assert_eq!(
                rx.recv().await.expect("command event should be delivered"),
                command_event
            );

            let failure_event = ConversationEvent::NotificationDeliveryFailed {
                detail: "No model is chosen for this conversation.".to_string(),
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
pub use server::{has_channel, subscriber_count};

#[cfg(test)]
mod wire_tests {
    #[test]
    fn test_a_preview_label_names_the_port_or_the_container() {
        let preview = |host: Option<&str>| SandboxPreview {
            port: 3000,
            host: host.map(str::to_string),
            url: String::new(),
        };
        assert_eq!(preview(None).label(), "port 3000");
        assert_eq!(preview(Some("172.21.0.2")).label(), "172.21.0.2:3000");
    }

    use super::*;

    #[test]
    fn test_app_events_round_trip_through_json() {
        let event = AppEvent::PodsChanged;
        let json = serde_json::to_string(&event).expect("serialize");
        assert_eq!(json, r#"{"type":"PodsChanged"}"#);
        assert_eq!(serde_json::from_str::<AppEvent>(&json).expect("deserialize"), event);
        for event in [AppEvent::TurnsChanged, AppEvent::ProvidersChanged, AppEvent::QuestionsChanged] {
            let json = serde_json::to_string(&event).expect("serialize");
            assert_eq!(serde_json::from_str::<AppEvent>(&json).expect("deserialize"), event);
        }
    }

    /// A tab still running an older bundle gets event types added since.
    /// It skips them rather than taking the stream for broken (SME-43).
    #[test]
    fn test_an_event_type_added_later_decodes_as_unknown() {
        for json in [r#"{"type":"AddedLater"}"#, r#"{"type":"AddedLater","x":[1,2],"y":{"z":"w"}}"#] {
            assert_eq!(
                serde_json::from_str::<ConversationEvent>(json).expect("conversation event"),
                ConversationEvent::Unknown,
                "{json}"
            );
            assert_eq!(
                serde_json::from_str::<AppEvent>(json).expect("app event"),
                AppEvent::Unknown,
                "{json}"
            );
        }
    }

    fn one_of_each() -> Vec<ConversationEvent> {
        vec![
            ConversationEvent::TurnError { message: "boom".to_string() },
            ConversationEvent::ReplyReset {},
            ConversationEvent::ReplyDelta { text: "Hi".to_string(), offset: 0 },
            ConversationEvent::TurnState { running: true },
            ConversationEvent::PodsChanged {},
            ConversationEvent::TurnsChanged {},
            ConversationEvent::QuestionsChanged {},
            ConversationEvent::QuestionUpdate { question: None },
            ConversationEvent::QuestionUpdate {
                question: Some(crate::questions::PendingQuestion {
                    tool_use_id: "toolu_q".to_string(),
                    questions: vec![crate::questions::Question {
                        question: "Delete it?".to_string(),
                        header: "Delete".to_string(),
                        options: vec![
                            crate::questions::QuestionOption { label: "Yes".to_string(), description: None },
                            crate::questions::QuestionOption { label: "No".to_string(), description: Some("keep it".to_string()) },
                        ],
                        multi_select: false,
                    }],
                }),
            },
            ConversationEvent::ModelChanged {},
            ConversationEvent::ProvidersChanged {},
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
            ConversationEvent::SandboxPreviewUpdate {
                pod_id: 1,
                previews: vec![
                    SandboxPreview {
                        port: 3000,
                        host: None,
                        url: "http://3000-3.preview.localhost:8181".to_string(),
                    },
                    SandboxPreview {
                        port: 3000,
                        host: Some("172.21.0.2".to_string()),
                        url: "http://172-21-0-2-3000-3.preview.localhost:8181".to_string(),
                    },
                ],
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
                position: None,
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
            ConversationEvent::ReposUpdate {
                repos: vec![crate::git::RepoSummary {
                    id: 1,
                    url: "git@github.com:o/r.git".to_string(),
                    path: "/workspace/r".to_string(),
                    requested_branch: Some("dev".to_string()),
                    branch: Some("dev".to_string()),
                    commit: Some("abc123".to_string()),
                    status: crate::git::RepoStatus::Failed,
                    error: Some("fatal: nope".to_string()),
                    agents_files: vec![],
                    loaded_instructions: vec![],
                    trust_requests: vec![],
                }],
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
