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
    #[cfg(test)]
    refuse_an_id_test_databases_share(conversation_id);
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
    #[cfg(test)]
    RECORDED_APP.with(|recorded| {
        if let Some(events) = recorded.borrow_mut().as_mut() {
            events.push(event.clone());
        }
    });
    let _ = APP_BUS.send(event);
}

#[cfg(test)]
thread_local! {
    static RECORDED_APP: std::cell::RefCell<Option<Vec<super::AppEvent>>> = const { std::cell::RefCell::new(None) };
}

/// The app-wide events published on this thread while it lives (SME-246
/// review 1). The bus is shared by every test in the process, so a test
/// that listens on it can pass on another test's event; a
/// `#[sqlx::test]` runs on a current-thread runtime, so what this
/// records is what the test's code published on that runtime (inline, or
/// from a task spawned onto it that ran before `events`); a publish from
/// another thread is missed, which fails a test rather than passing one.
/// One recording at a time per thread: dropping one ends any other.
#[cfg(test)]
pub struct AppRecording(());

#[cfg(test)]
impl AppRecording {
    pub fn start() -> Self {
        RECORDED_APP.with(|recorded| *recorded.borrow_mut() = Some(Vec::new()));
        AppRecording(())
    }

    /// What was published so far, in order.
    pub fn events(&self) -> Vec<super::AppEvent> {
        RECORDED_APP.with(|recorded| recorded.borrow().clone().unwrap_or_default())
    }
}

#[cfg(test)]
impl Drop for AppRecording {
    fn drop(&mut self) {
        RECORDED_APP.with(|recorded| *recorded.borrow_mut() = None);
    }
}

/// Subscribes to app-wide events from this point forward.
pub fn subscribe_app() -> broadcast::Receiver<super::AppEvent> {
    APP_BUS.subscribe()
}

/// Below this, a conversation id is one every `#[sqlx::test]`
/// database hands out: each numbers conversations from 1, and tests run
/// alongside each other in one process, so two tests' "conversation 1"
/// are one channel. No test database gets near it; the ids tests pick
/// by hand, and `db::test_support::start_ids_clear_of_other_runs`'s
/// bases, are far above it (SME-135).
#[cfg(test)]
pub(crate) const TEST_SHARED_IDS_END: i64 = 100_000;

/// In tests, refuses to listen on an id another test's database also
/// hands out: what such a test hears, or counts, depends on what runs
/// alongside it. Before the map's lock, so the panic can't poison it.
#[cfg(test)]
fn refuse_an_id_test_databases_share(conversation_id: i64) {
    assert!(
        conversation_id >= TEST_SHARED_IDS_END,
        "conversation {conversation_id}: tests subscribe only on a conversation id of their own; \
         see docs/testing.md, Tests that touch per-conversation state"
    );
}

/// How many live subscriptions `conversation_id` has.
#[cfg(test)]
pub fn subscriber_count(conversation_id: i64) -> usize {
    refuse_an_id_test_databases_share(conversation_id);
    BUSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&conversation_id)
        .map_or(0, |sender| sender.receiver_count())
}

/// Whether `conversation_id` has a channel at all.
#[cfg(test)]
pub fn has_channel(conversation_id: i64) -> bool {
    refuse_an_id_test_databases_share(conversation_id);
    BUSES.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&conversation_id)
}

#[cfg(test)]
mod tests;
