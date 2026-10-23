//! Points where a test can pause an agent's start or a step around it, to
//! land a stop, a delete or a send exactly there. Each is keyed by a
//! conversation id: the hook's own doc says whose.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::oneshot;

type Pause = (oneshot::Sender<()>, oneshot::Receiver<()>);
static PAUSES: LazyLock<Mutex<HashMap<(&'static str, i64), Pause>>> = LazyLock::new(Default::default);

/// Pauses the next pass of hook `hook` for `id`. The first receiver fires
/// once it's paused there; sending on the second lets it go on.
pub fn pause(hook: &'static str, id: i64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    PAUSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((hook, id), (reached_tx, release_rx));
    (reached_rx, release_tx)
}

/// Where the code passes hook `hook` for `id`: waits there if a test
/// paused it.
pub async fn reach(hook: &'static str, id: i64) {
    let pause = PAUSES.lock().unwrap_or_else(|e| e.into_inner()).remove(&(hook, id));
    if let Some((reached, release)) = pause {
        let _ = reached.send(());
        let _ = release.await;
    }
}

/// The start of a child of parent `id`, before its done step.
pub const BEFORE_START_DONE: &str = "before_start_done";
