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

/// Pauses `conversation_id`'s next turn right after it's counted as in
/// flight (`TurnInFlight::start`), before it waits for the turn lock. As
/// `pause_after_message_saved`.
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
