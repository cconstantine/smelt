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

/// The start of a child of parent `id`, after the trust is read again and
/// before its AGENTS.md is loaded.
pub const BEFORE_AGENTS_MD_LOAD: &str = "before_agents_md_load";

static START_FAILURES: LazyLock<Mutex<HashMap<i64, String>>> = LazyLock::new(Default::default);

/// Makes the next start of a child of parent `id` fail with `error`.
pub fn fail_next_start(id: i64, error: &str) {
    START_FAILURES.lock().unwrap_or_else(|e| e.into_inner()).insert(id, error.to_string());
}

/// The failure a test asked of the next start of a child of parent `id`.
pub fn injected_start_failure(id: i64) -> Option<String> {
    START_FAILURES.lock().unwrap_or_else(|e| e.into_inner()).remove(&id)
}

static STARTS_ENDED: LazyLock<Mutex<std::collections::HashSet<i64>>> = LazyLock::new(Default::default);

/// Records that child `id`'s start task has ended, however it ended.
pub fn start_ended(id: i64) {
    STARTS_ENDED.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
}

/// Whether child `id`'s start task has ended.
pub fn starts_ended(id: i64) -> bool {
    STARTS_ENDED.lock().unwrap_or_else(|e| e.into_inner()).contains(&id)
}

static LAST_TURNS_CHECKED: LazyLock<Mutex<HashMap<i64, usize>>> = LazyLock::new(Default::default);

/// Counts an `after_last_turn` check of `id` once dropped, however it ended.
pub struct Checked(pub i64);

impl Drop for Checked {
    fn drop(&mut self) {
        *LAST_TURNS_CHECKED.lock().unwrap_or_else(|e| e.into_inner()).entry(self.0).or_default() += 1;
    }
}

/// How many `after_last_turn` checks of `id` have ended.
pub fn last_turns_checked(id: i64) -> usize {
    LAST_TURNS_CHECKED.lock().unwrap_or_else(|e| e.into_inner()).get(&id).copied().unwrap_or(0)
}
