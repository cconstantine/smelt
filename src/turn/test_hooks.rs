//! Points where a test can pause a turn, to land a Stop exactly there.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::oneshot;

type Pause = (oneshot::Sender<()>, oneshot::Receiver<()>);
static AFTER_MESSAGE_SAVED: LazyLock<Mutex<HashMap<i64, Pause>>> = LazyLock::new(Default::default);

/// Pauses `conversation_id`'s next turn right after it saves its own
/// message. The first receiver fires once it's paused there; sending
/// on the second lets it go on.
pub fn pause_after_message_saved(conversation_id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    AFTER_MESSAGE_SAVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conversation_id, (reached_tx, release_rx));
    (reached_rx, release_tx)
}

pub async fn after_message_saved(conversation_id: i64) {
    let pause = AFTER_MESSAGE_SAVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
    if let Some((reached, release)) = pause {
        let _ = reached.send(());
        let _ = release.await;
    }
}

static AFTER_TURN_COUNTED: LazyLock<Mutex<HashMap<i64, Pause>>> = LazyLock::new(Default::default);

/// Pauses `conversation_id`'s next turn after it's admitted: counted as
/// in flight by its caller (`TurnInFlight::start_by_user`, SME-216, or
/// `start_unless_paused`, SME-217; in a test's `run_turn`,
/// `TurnInFlight::start`), when its future first runs, before it waits
/// for the turn lock. As `pause_after_message_saved`.
pub fn pause_after_turn_counted(conversation_id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    AFTER_TURN_COUNTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conversation_id, (reached_tx, release_rx));
    (reached_rx, release_tx)
}

pub async fn after_turn_counted(conversation_id: i64) {
    let pause = AFTER_TURN_COUNTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
    if let Some((reached, release)) = pause {
        let _ = reached.send(());
        let _ = release.await;
    }
}

type SyncPause = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
static AFTER_IN_FLIGHT_CHANGED: LazyLock<Mutex<HashMap<i64, SyncPause>>> = LazyLock::new(Default::default);

/// Pauses the next change to `conversation_id`'s turns in flight that
/// checks this hook (a turn ending: `TurnInFlight`'s drop) right after
/// it, on whatever thread makes it. The first receiver fires once it's
/// paused there; sending on the second lets it go on. Synchronous, since
/// a drop can't await; the pause gives up after 5 s.
pub fn pause_after_in_flight_changed(
    conversation_id: i64,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    AFTER_IN_FLIGHT_CHANGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conversation_id, (reached_tx, release_rx));
    (reached_rx, release_tx)
}

pub fn after_in_flight_changed(conversation_id: i64) {
    let pause = AFTER_IN_FLIGHT_CHANGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
    if let Some((reached, release)) = pause {
        let _ = reached.send(());
        let _ = release.recv_timeout(std::time::Duration::from_secs(5));
    }
}

/// A pause point keyed by conversation, as `AFTER_MESSAGE_SAVED`.
type Pauses = LazyLock<Mutex<HashMap<i64, Pause>>>;

fn pause_at(pauses: &Pauses, conversation_id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    pauses.lock().unwrap_or_else(|e| e.into_inner()).insert(conversation_id, (reached_tx, release_rx));
    (reached_rx, release_tx)
}

async fn reach(pauses: &Pauses, conversation_id: i64) {
    let pause = pauses.lock().unwrap_or_else(|e| e.into_inner()).remove(&conversation_id);
    if let Some((reached, release)) = pause {
        let _ = reached.send(());
        let _ = release.await;
    }
}

static AFTER_REPLY_SAVED: Pauses = LazyLock::new(Default::default);

/// Pauses `conversation_id`'s next model call right after its reply is
/// saved, before its usage is recorded (SME-178). As
/// `pause_after_message_saved`.
pub fn pause_after_reply_saved(conversation_id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    pause_at(&AFTER_REPLY_SAVED, conversation_id)
}

pub async fn after_reply_saved(conversation_id: i64) {
    reach(&AFTER_REPLY_SAVED, conversation_id).await;
}

static DURING_RECORD: Pauses = LazyLock::new(Default::default);

/// Pauses `conversation_id`'s next finished call's record inside its
/// spawned task, before it writes (SME-178). As
/// `pause_after_message_saved`.
pub fn pause_during_record(conversation_id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    pause_at(&DURING_RECORD, conversation_id)
}

pub async fn during_record(conversation_id: i64) {
    reach(&DURING_RECORD, conversation_id).await;
}

static AFTER_NOTICE_ADMITTED: Pauses = LazyLock::new(Default::default);

/// Pauses `conversation_id`'s next wake or delivered notice right after
/// it's let in past the pause, before it waits for the turn lock
/// (SME-217). As `pause_after_message_saved`.
pub fn pause_after_notice_admitted(conversation_id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    pause_at(&AFTER_NOTICE_ADMITTED, conversation_id)
}

pub async fn after_notice_admitted(conversation_id: i64) {
    reach(&AFTER_NOTICE_ADMITTED, conversation_id).await;
}

static STOPPED_WHILE_QUEUED: LazyLock<Mutex<HashMap<(i64, String), Pause>>> = LazyLock::new(Default::default);

/// Pauses `conversation_id`'s turn whose message is `text` in its stop
/// arm, when a Stop has caught it queued, before it waits for the turn
/// lock again (SME-202). Keyed by the message's text too, so a test can
/// pause one named turn there and let another pass. As
/// `pause_after_message_saved`.
pub fn pause_stopped_while_queued(conversation_id: i64, text: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    STOPPED_WHILE_QUEUED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((conversation_id, text.to_string()), (reached_tx, release_rx));
    (reached_rx, release_tx)
}

pub async fn stopped_while_queued(conversation_id: i64, new_message: &Option<crate::anthropic::AnthropicMessage>) {
    let text = new_message.iter().flat_map(|m| &m.content).find_map(|block| match block {
        crate::anthropic::ContentBlock::Text { text } => Some(text.clone()),
        _ => None,
    });
    let Some(text) = text else { return };
    let pause = STOPPED_WHILE_QUEUED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(conversation_id, text));
    if let Some((reached, release)) = pause {
        let _ = reached.send(());
        let _ = release.await;
    }
}
