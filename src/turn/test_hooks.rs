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
