//! Each conversation's turn state, kept in memory: its turn lock, the
//! reply streaming, stops, the pause after a stop, turns in flight and the
//! last failed turn's error.

use super::*;

/// A live `send_message` call and a notice's `run_turn` call (a finished
/// command waking the model, say, or two notices at once) can race for the same
/// conversation — Anthropic's strict user/assistant alternation breaks if
/// two writers persist a turn at once. Keyed by conversation id; which
/// caller acquires a given conversation's lock first when several are ready
/// is unspecified (see SME-8's Open questions).
#[cfg(feature = "server")]
pub(super) static CONVERSATION_LOCKS: LazyLock<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "server")]
pub(crate) fn conversation_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = CONVERSATION_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks
        .entry(conversation_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Drops `conversation_id`'s lock, for when the conversation is deleted.
/// A turn still holding it keeps its own handle; any later turn gets a
/// fresh lock and then fails, since the conversation no longer exists.
#[cfg(feature = "server")]
pub(crate) fn forget_conversation_lock(conversation_id: i64) {
    TURN_ERRORS.lock().unwrap_or_else(|e| e.into_inner()).remove(&conversation_id);
    CONVERSATION_LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
    TURN_STOPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
}

/// The reply text streamed so far in each conversation's current model
/// call, for a tab that connects mid-reply (`get_reply_in_progress`).
/// Cleared when a call starts, when its reply is saved, and when the turn
/// ends.
#[cfg(feature = "server")]
pub(super) static REPLIES_IN_PROGRESS: LazyLock<Mutex<HashMap<i64, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `conversation_id`'s reply so far, if a model call is streaming one.
#[cfg(feature = "server")]
pub(crate) fn reply_in_progress(conversation_id: i64) -> Option<String> {
    REPLIES_IN_PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&conversation_id)
        .filter(|text| !text.is_empty())
        .cloned()
}

/// Ends `conversation_id`'s reply in progress (see `REPLIES_IN_PROGRESS`).
#[cfg(feature = "server")]
pub(super) fn clear_reply_in_progress(conversation_id: i64) {
    REPLIES_IN_PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
}

/// Per conversation, a counter bumped each time the user stops its turn.
/// A turn notes the value when it starts (before waiting for the turn
/// lock) and ends as soon as it changes, so a stop ends the running turn
/// and any queued behind it, but not turns that start afterwards.
#[cfg(feature = "server")]
pub(super) static TURN_STOPS: LazyLock<Mutex<HashMap<i64, TurnStops>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A conversation's stop counter, and the last stop already noted in it
/// (`record_stop`), so one stop is noted once however many turns it ends.
#[cfg(feature = "server")]
pub(super) struct TurnStops {
    pub(super) counter: tokio::sync::watch::Sender<u64>,
    pub(super) noted: u64,
}

#[cfg(feature = "server")]
impl Default for TurnStops {
    fn default() -> Self {
        TurnStops { counter: tokio::sync::watch::channel(0).0, noted: 0 }
    }
}

/// A receiver for `conversation_id`'s stop counter, with its current value
/// already seen, so only a later stop wakes it.
#[cfg(feature = "server")]
pub(super) fn stop_receiver(conversation_id: i64) -> tokio::sync::watch::Receiver<u64> {
    let mut stops = TURN_STOPS.lock().unwrap_or_else(|e| e.into_inner());
    let mut receiver = stops.entry(conversation_id).or_default().counter.subscribe();
    receiver.borrow_and_update();
    receiver
}

/// Stops `conversation_id`'s running turn, and any queued behind it. The
/// pod, its terminals and running commands are left alone. A no-op when
/// nothing is running.
#[cfg(feature = "server")]
pub(crate) fn stop_turn_now(conversation_id: i64) {
    // Paused even when nothing is running: the user asked the model to
    // stop, so a command finishing right after shouldn't start it again.
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conversation_id);
    let stops = TURN_STOPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(stop) = stops.get(&conversation_id) {
        stop.counter.send_modify(|count| *count += 1);
    }
}

/// Conversations the user has stopped and not written in since. While
/// paused, a finished command or another notice doesn't wake the model:
/// its notice is still saved, and the model sees it on the user's next
/// message. Otherwise a stop would be undone seconds later by whatever was
/// still running. In memory: a restart un-pauses, which is harmless.
#[cfg(feature = "server")]
pub(super) static PAUSED: LazyLock<Mutex<std::collections::HashSet<i64>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Whether `conversation_id` is paused after a stop (see `PAUSED`).
#[cfg(feature = "server")]
pub(crate) fn is_paused(conversation_id: i64) -> bool {
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&conversation_id)
}

/// Ends the pause after a stop: the user has written again.
#[cfg(feature = "server")]
pub(super) fn resume_turns(conversation_id: i64) {
    PAUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conversation_id);
}

/// How many turns are running or queued, per conversation, for the Stop
/// button (`TurnState`).
#[cfg(feature = "server")]
pub(super) static TURNS_IN_FLIGHT: LazyLock<Mutex<HashMap<i64, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Counts a turn as in flight for as long as it lives, including when it's
/// dropped part-way by a stop, and publishes `TurnState` when the
/// conversation goes from idle to busy or back.
#[cfg(feature = "server")]
pub(super) struct TurnInFlight(i64);

#[cfg(feature = "server")]
impl TurnInFlight {
    pub(super) fn start(conversation_id: i64) -> Self {
        let first = {
            let mut counts = TURNS_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            let count = counts.entry(conversation_id).or_insert(0);
            *count += 1;
            *count == 1
        };
        if first {
            crate::events::publish(
                conversation_id,
                crate::events::ConversationEvent::TurnState { running: true },
            );
            crate::events::publish_app(crate::events::AppEvent::TurnsChanged);
        }
        TurnInFlight(conversation_id)
    }
}

#[cfg(feature = "server")]
impl Drop for TurnInFlight {
    fn drop(&mut self) {
        let last = {
            let mut counts = TURNS_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            match counts.get_mut(&self.0) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    false
                }
                _ => {
                    counts.remove(&self.0);
                    true
                }
            }
        };
        if last {
            // A stopped or failed turn leaves no reply in progress.
            clear_reply_in_progress(self.0);
            release_idle_turn_state(self.0);
            crate::events::publish(
                self.0,
                crate::events::ConversationEvent::TurnState { running: false },
            );
            crate::events::publish_app(crate::events::AppEvent::TurnsChanged);
        }
    }
}

/// Frees `conversation_id`'s turn lock and stop counter once no one has
/// them (SME-91): the lock only when the map's is the last handle, so one
/// in use is never replaced by a fresh one, and the stop counter when no
/// turn listens to it (a stop with nothing running only pauses). Each is
/// made again on next use. Under each map's lock, which handing one out
/// also takes. Not the kept error's entry: a turn starting meanwhile has
/// its generation there already (SME-91 review); it's a few bytes per
/// conversation, freed on delete.
#[cfg(feature = "server")]
pub(super) fn release_idle_turn_state(conversation_id: i64) {
    {
        let mut locks = CONVERSATION_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        if locks.get(&conversation_id).is_some_and(|lock| Arc::strong_count(lock) == 1) {
            locks.remove(&conversation_id);
        }
    }
    let mut stops = TURN_STOPS.lock().unwrap_or_else(|e| e.into_inner());
    if stops.get(&conversation_id).is_some_and(|stops| stops.counter.receiver_count() == 0) {
        stops.remove(&conversation_id);
    }
}

/// Every conversation with a turn running or queued, for the sidebar's
/// "working" marks (SME-41 D9).
#[cfg(feature = "server")]
pub(crate) fn busy_conversations() -> Vec<i64> {
    TURNS_IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect()
}

/// Whether `conversation_id` has a turn running or queued.
#[cfg(feature = "server")]
pub(crate) fn turn_running(conversation_id: i64) -> bool {
    TURNS_IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&conversation_id)
}

/// Each conversation's last failed turn's error, until the user writes
/// again, for a tab that connects after the `TurnError` event (SME-51 B11).
#[cfg(feature = "server")]
pub(super) static TURN_ERRORS: LazyLock<Mutex<HashMap<i64, KeptTurnError>>> = LazyLock::new(Default::default);

/// A conversation's kept error, and its turn generation: bumped by the
/// user sending and by every turn starting, so a turn's failure is kept
/// only if no newer turn has started since it did (SME-91).
#[cfg(feature = "server")]
#[derive(Default)]
pub(super) struct KeptTurnError {
    generation: u64,
    error: Option<String>,
}

/// Starts a new turn generation for `conversation_id`, clearing its kept
/// error, and returns the new generation.
#[cfg(feature = "server")]
pub(super) fn new_turn_generation(conversation_id: i64) -> u64 {
    let mut errors = TURN_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    let kept = errors.entry(conversation_id).or_default();
    kept.generation += 1;
    kept.error = None;
    kept.generation
}

/// Keeps `error` as `conversation_id`'s, if `generation` is still its
/// latest turn generation.
#[cfg(feature = "server")]
pub(super) fn keep_turn_error(conversation_id: i64, generation: u64, error: String) {
    let mut errors = TURN_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(kept) = errors.get_mut(&conversation_id)
        && kept.generation == generation
    {
        kept.error = Some(error);
    }
}

/// Shows `message` as `conversation_id`'s error, as a failed turn's is
/// (`TurnError`, and kept for a reconnecting tab until the user writes),
/// for something that failed outside a turn: a "Work on a repo" that
/// couldn't start the sandbox (SME-91).
#[cfg(feature = "server")]
pub(crate) fn show_conversation_error(conversation_id: i64, message: String) {
    TURN_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(conversation_id)
        .or_default()
        .error = Some(message.clone());
    crate::events::publish(conversation_id, crate::events::ConversationEvent::TurnError { message });
}

#[cfg(test)]
pub(super) fn remember_turn_error(conversation_id: i64, error: Option<String>) {
    TURN_ERRORS.lock().unwrap_or_else(|e| e.into_inner()).entry(conversation_id).or_default().error = error;
}

#[cfg(feature = "server")]
pub(crate) fn last_turn_error(conversation_id: i64) -> Option<String> {
    TURN_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&conversation_id)
        .and_then(|kept| kept.error.clone())
}
