//! Per-conversation live event bus — the shared home for pushing updates
//! that happen with no `send_message` request in flight (a background
//! task's tick or completion), so neither `anthropic::tools` nor `api::chat`
//! has to depend on the other to publish or read these. `tools.rs` calls
//! `publish`; `chat.rs` calls both `publish` (after persisting a batch of
//! rows) and `subscribe` (to relay everything to a browser tab).

use serde::{Deserialize, Serialize};

use crate::models::Message;

/// `TaskUpdate` is ephemeral UI telemetry, regenerable at any time from the
/// task registry (`anthropic::tools::snapshot_tasks`) — never persisted.
/// `MessagesAppended` carries no new data of its own; it's a live-delivery
/// notification for rows `db::create_message` already persisted. The three
/// `Sandbox*` variants are the same kind of ephemeral UI telemetry as
/// `TaskUpdate`, regenerable at any time from `api::chat::get_sandbox_state`
/// — see `docs/projects/completed/20260815-sandbox-visibility.md`.
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
    MessagesAppended(Vec<Message>),
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
}

#[cfg(feature = "server")]
mod server {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use tokio::sync::broadcast;

    use super::ConversationEvent;

    /// Bound on how many events a lagging subscriber can fall behind by
    /// before `broadcast` starts dropping its oldest ones — generous for
    /// this stage's traffic (a handful of task ticks and message batches),
    /// not a tuned production value.
    const CHANNEL_CAPACITY: usize = 64;

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

    #[cfg(test)]
    mod tests {
        use super::*;

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
            assert_eq!(rx.recv().await.expect("pod event should be delivered"), pod_event);

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
        }
    }
}

#[cfg(feature = "server")]
pub use server::{publish, subscribe};
