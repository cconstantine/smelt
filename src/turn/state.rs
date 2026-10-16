//! Each conversation's turn state, kept in memory: its turn lock, the
//! reply streaming, stops, the pause after a stop, turns in flight and the
//! last failed turn's error. One map, one entry per conversation (SME-52).

use super::*;

/// Every conversation's turn state. One `std` mutex, taken briefly and
/// never across an `.await`.
static RUNTIMES: LazyLock<Mutex<HashMap<i64, ConversationRuntime>>> = LazyLock::new(Default::default);

/// A conversation's turn state. An entry is made on first use. Its lock,
/// stop counter and reply are freed when idle (`release_idle_turn_state`);
/// the entry itself goes only when nothing in it is in use, which, once a
/// turn has run (its generation), is when the conversation is deleted: a
/// few bytes per conversation, as the separate kept-error map was. A turn
/// that finds its conversation already deleted forgets its generation
/// (SME-179).
#[derive(Default)]
pub(super) struct ConversationRuntime {
    /// The turn lock. A live `send_message` call and a notice's `run_turn`
    /// call (a finished command waking the model, say, or two notices at
    /// once) can race for the same conversation — Anthropic's strict
    /// user/assistant alternation breaks if two writers persist a turn at
    /// once. Which caller acquires it first when several are ready is
    /// unspecified (see SME-8's Open questions).
    lock: Option<Arc<tokio::sync::Mutex<()>>>,
    /// The reply text streamed so far in the current model call, for a tab
    /// that connects mid-reply (`get_reply_in_progress`). Cleared when a
    /// call starts, when its reply is saved, when its turn fails or is
    /// stopped, and when the last turn ends.
    reply: Option<String>,
    /// The stop counter and the last stop noted (`TurnStops`).
    stops: Option<TurnStops>,
    /// Stopped by the user and not written in since. While paused, a
    /// finished command or another notice doesn't wake the model: its
    /// notice is still saved, and the model sees it on the user's next
    /// message. Otherwise a stop would be undone seconds later by whatever
    /// was still running. In memory: a restart un-pauses, which is
    /// harmless.
    paused: bool,
    /// How many turns are running or queued, for the Stop button
    /// (`TurnState`).
    turns_in_flight: usize,
    /// When the current busy spell (`turns_in_flight` going 0 to 1)
    /// started, on a monotonic clock: `Some` exactly while a turn runs, so
    /// the "Working…" timer counts from the spell's start rather than from
    /// when a tab connected (SME-160). A turn queued behind a running one
    /// doesn't move it; the last turn ending does.
    started_at: Option<std::time::Instant>,
    /// The last failed turn's error and the turn generation.
    kept: KeptTurnError,
}

impl ConversationRuntime {
    /// A receiver for the stop counter, with its current value already
    /// seen, so only a later stop wakes it.
    fn stop_receiver(&mut self) -> tokio::sync::watch::Receiver<u64> {
        let mut receiver = self.stops.get_or_insert_with(TurnStops::default).counter.subscribe();
        receiver.borrow_and_update();
        receiver
    }

    /// Frees the turn lock and stop counter once no one has them (SME-91):
    /// the lock only when this is the last handle, so one in use is never
    /// replaced by a fresh one, and the stop counter when no turn listens
    /// to it (a stop with nothing running only pauses). Each is made again
    /// on next use. Not the kept error: a turn starting meanwhile has its
    /// generation there already (SME-91 review).
    fn release_idle(&mut self) {
        if self.lock.as_ref().is_some_and(|lock| Arc::strong_count(lock) == 1) {
            self.lock = None;
        }
        if self.stops.as_ref().is_some_and(|stops| stops.counter.receiver_count() == 0) {
            self.stops = None;
        }
    }

    fn is_idle(&self) -> bool {
        self.lock.is_none()
            && self.reply.is_none()
            && self.stops.is_none()
            && !self.paused
            && self.turns_in_flight == 0
            && self.started_at.is_none()
            && self.kept.generation == 0
            && self.kept.error.is_none()
    }
}

/// Runs `f` on `conversation_id`'s turn state, making it if need be.
fn with_runtime<R>(conversation_id: i64, f: impl FnOnce(&mut ConversationRuntime) -> R) -> R {
    let mut runtimes = RUNTIMES.lock().unwrap_or_else(|e| e.into_inner());
    f(runtimes.entry(conversation_id).or_default())
}

/// Reads `conversation_id`'s turn state, if it has any.
fn read_runtime<R>(conversation_id: i64, f: impl FnOnce(&ConversationRuntime) -> R) -> Option<R> {
    RUNTIMES.lock().unwrap_or_else(|e| e.into_inner()).get(&conversation_id).map(f)
}

/// Runs `f` on `conversation_id`'s turn state if it has any, then removes
/// the entry if nothing in it is in use any more.
fn update_existing(conversation_id: i64, f: impl FnOnce(&mut ConversationRuntime)) {
    let mut runtimes = RUNTIMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(runtime) = runtimes.get_mut(&conversation_id) {
        f(runtime);
        if runtime.is_idle() {
            runtimes.remove(&conversation_id);
        }
    }
}

pub(crate) fn conversation_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    with_runtime(conversation_id, |runtime| {
        runtime
            .lock
            .get_or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    })
}

/// Drops `conversation_id`'s lock, stop counter, pause and kept error, for
/// when the conversation is deleted. A turn still holding the lock keeps
/// its own handle; any later turn gets a fresh lock and then fails, since
/// the conversation no longer exists. A turn still in flight keeps its
/// place in the count, and its reply, until it ends.
pub(crate) fn forget_conversation_lock(conversation_id: i64) {
    update_existing(conversation_id, |runtime| {
        runtime.lock = None;
        runtime.stops = None;
        runtime.paused = false;
        runtime.kept = KeptTurnError::default();
    });
}

/// `conversation_id`'s reply so far, if a model call is streaming one.
pub(crate) fn reply_in_progress(conversation_id: i64) -> Option<String> {
    read_runtime(conversation_id, |runtime| runtime.reply.clone())
        .flatten()
        .filter(|text| !text.is_empty())
}

/// Ends `conversation_id`'s reply in progress.
pub(super) fn clear_reply_in_progress(conversation_id: i64) {
    update_existing(conversation_id, |runtime| runtime.reply = None);
}

/// Adds `delta` to `conversation_id`'s reply so far and calls `publish`
/// with where it starts in it, while still holding the state that
/// `get_reply_in_progress` reads through, so a tab's fetched text and the
/// offsets it then receives agree (SME-51 B3).
pub(super) fn append_reply(conversation_id: i64, delta: &str, publish: impl FnOnce(usize)) {
    with_runtime(conversation_id, |runtime| {
        let reply = runtime.reply.get_or_insert_with(String::new);
        let offset = reply.len();
        reply.push_str(delta);
        publish(offset);
    });
}

/// Per conversation, a counter bumped each time the user stops its turn.
/// A turn notes the value when it starts (before waiting for the turn
/// lock) and ends as soon as it changes, so a stop ends the running turn
/// and any queued behind it, but not turns that start afterwards. With
/// the last stop already noted (`record_stop`), so one stop is noted once
/// however many turns it ends.
pub(super) struct TurnStops {
    counter: tokio::sync::watch::Sender<u64>,
    noted: u64,
}

impl Default for TurnStops {
    fn default() -> Self {
        TurnStops { counter: tokio::sync::watch::channel(0).0, noted: 0 }
    }
}

/// A receiver for `conversation_id`'s stop counter, with its current value
/// already seen, so only a later stop wakes it.
pub(super) fn stop_receiver(conversation_id: i64) -> tokio::sync::watch::Receiver<u64> {
    with_runtime(conversation_id, ConversationRuntime::stop_receiver)
}

/// Notes stop `stopped_by` (the stop counter's value) in
/// `conversation_id`, and says whether it's the first time.
pub(super) fn note_stop(conversation_id: i64, stopped_by: u64) -> bool {
    with_runtime(conversation_id, |runtime| {
        let stops = runtime.stops.get_or_insert_with(TurnStops::default);
        let first = stopped_by > stops.noted;
        stops.noted = stops.noted.max(stopped_by);
        first
    })
}

/// Stops `conversation_id`'s running turn, and any queued behind it. The
/// pod, its terminals and running commands are left alone. A no-op when
/// nothing is running.
pub(crate) fn stop_turn_now(conversation_id: i64) {
    with_runtime(conversation_id, |runtime| {
        // Paused even when nothing is running: the user asked the model to
        // stop, so a command finishing right after shouldn't start it again.
        runtime.paused = true;
        if let Some(stops) = &runtime.stops {
            stops.counter.send_modify(|count| *count += 1);
        }
    });
}

/// Whether `conversation_id` is paused after a stop (see
/// `ConversationRuntime::paused`).
pub(crate) fn is_paused(conversation_id: i64) -> bool {
    read_runtime(conversation_id, |runtime| runtime.paused).unwrap_or(false)
}

/// Ends the pause after a stop: the user has written again.
pub(super) fn resume_turns(conversation_id: i64) {
    update_existing(conversation_id, |runtime| runtime.paused = false);
}

/// Counts a turn as in flight for as long as it lives, including when it's
/// dropped part-way by a stop, and publishes `TurnState` when the
/// conversation goes from idle to busy or back. It holds the turn's stop
/// receiver, made when it's counted.
///
/// Its start and its end are each one critical section of the turn
/// state, publishing `TurnState` inside it (SME-179 B29): a turn counted
/// as another ends comes wholly before or after that end, so tabs are
/// told `running: false` and `running: true` in the order they happened,
/// the end can't clear the new turn's reply, and a Stop can't land
/// between the count and the receiver. `events::publish` only sends on a
/// broadcast channel, under its own map's lock (the same order
/// `append_reply` takes them in).
pub(super) struct TurnInFlight {
    conversation_id: i64,
    /// `None` only once dropped.
    stop: Option<tokio::sync::watch::Receiver<u64>>,
}

impl TurnInFlight {
    pub(super) fn start(conversation_id: i64) -> Self {
        let (first, stop) = with_runtime(conversation_id, |runtime| {
            runtime.turns_in_flight += 1;
            let stop = runtime.stop_receiver();
            let first = runtime.turns_in_flight == 1;
            if first {
                runtime.started_at = Some(std::time::Instant::now());
                crate::events::publish(
                    conversation_id,
                    crate::events::ConversationEvent::TurnState { running: true, elapsed: 0 },
                );
            }
            (first, stop)
        });
        if first {
            // Only tells the sidebar to fetch the busy list again.
            crate::events::publish_app(crate::events::AppEvent::TurnsChanged);
        }
        TurnInFlight { conversation_id, stop: Some(stop) }
    }

    /// The turn's stop receiver: it changes when the user stops the
    /// conversation after the turn was counted.
    pub(super) fn stop(&mut self) -> &mut tokio::sync::watch::Receiver<u64> {
        let conversation_id = self.conversation_id;
        self.stop.get_or_insert_with(|| stop_receiver(conversation_id))
    }
}

impl Drop for TurnInFlight {
    fn drop(&mut self) {
        let conversation_id = self.conversation_id;
        let stop = self.stop.take();
        let mut last = false;
        update_existing(conversation_id, |runtime| {
            // First, so the release below sees no listener of this turn's:
            // the last turn ending frees the stop counter.
            drop(stop);
            runtime.turns_in_flight = runtime.turns_in_flight.saturating_sub(1);
            last = runtime.turns_in_flight == 0;
            if last {
                runtime.started_at = None;
                // A stopped or failed turn leaves no reply in progress.
                runtime.reply = None;
                runtime.release_idle();
                crate::events::publish(
                    conversation_id,
                    crate::events::ConversationEvent::TurnState { running: false, elapsed: 0 },
                );
            }
        });
        #[cfg(test)]
        super::test_hooks::after_in_flight_changed(conversation_id);
        if last {
            crate::events::publish_app(crate::events::AppEvent::TurnsChanged);
        }
    }
}

/// Frees `conversation_id`'s turn lock and stop counter once no one has
/// them (`ConversationRuntime::release_idle`), under the state's lock,
/// which handing one out also takes. The kept error stays: it's a few
/// bytes per conversation, freed on delete.
pub(super) fn release_idle_turn_state(conversation_id: i64) {
    update_existing(conversation_id, ConversationRuntime::release_idle);
}

/// Every conversation with a turn running or queued, for the sidebar's
/// "working" marks (SME-41 D9).
pub(crate) fn busy_conversations() -> Vec<i64> {
    RUNTIMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(_, runtime)| runtime.turns_in_flight > 0)
        .map(|(id, _)| *id)
        .collect()
}

/// Whether `conversation_id` has a turn running or queued.
pub(crate) fn turn_running(conversation_id: i64) -> bool {
    read_runtime(conversation_id, |runtime| runtime.turns_in_flight > 0).unwrap_or(false)
}

/// How long `conversation_id`'s busy spell has been running, or `None`
/// while no turn is: the "Working…" timer's age, counted on a monotonic
/// clock from the spell's start, so a tab that connects (or reloads)
/// mid-turn shows the turn's real age rather than starting over
/// (SME-160).
pub(crate) fn turn_started_elapsed(conversation_id: i64) -> Option<std::time::Duration> {
    read_runtime(conversation_id, |runtime| runtime.started_at)
        .flatten()
        .map(|started| started.elapsed())
}

/// A conversation's last failed turn's error, until the user writes again,
/// for a tab that connects after the `TurnError` event (SME-51 B11), and
/// its turn generation: bumped by the user sending or answering, by a turn
/// with a message of its own or an answer once it holds the turn lock, and
/// by a wake once it has notices for the model, so a turn's failure is
/// kept only if no newer turn has started since it did (SME-91). A wake
/// with nothing to do leaves it alone (SME-179).
#[derive(Default)]
pub(super) struct KeptTurnError {
    generation: u64,
    error: Option<String>,
}

/// Starts a new turn generation for `conversation_id`, clearing its kept
/// error, and returns the new generation.
pub(super) fn new_turn_generation(conversation_id: i64) -> u64 {
    with_runtime(conversation_id, |runtime| {
        runtime.kept.generation += 1;
        runtime.kept.error = None;
        runtime.kept.generation
    })
}

/// Forgets `conversation_id`'s turn generation and kept error, for a turn
/// that found its conversation deleted: the generation it took would
/// otherwise keep the conversation's turn state for good (SME-179 B31).
pub(super) fn forget_turn_generation(conversation_id: i64) {
    update_existing(conversation_id, |runtime| runtime.kept = KeptTurnError::default());
}

/// Keeps `error` as `conversation_id`'s, if `generation` is still its
/// latest turn generation.
pub(super) fn keep_turn_error(conversation_id: i64, generation: u64, error: String) {
    update_existing(conversation_id, |runtime| {
        if runtime.kept.generation == generation {
            runtime.kept.error = Some(error);
        }
    });
}

/// Shows `message` as `conversation_id`'s error, as a failed turn's is
/// (`TurnError`, and kept for a reconnecting tab until the user writes),
/// for something that failed outside a turn: a "Work on a repo" that
/// couldn't start the sandbox (SME-91).
pub(crate) fn show_conversation_error(conversation_id: i64, message: String) {
    with_runtime(conversation_id, |runtime| runtime.kept.error = Some(message.clone()));
    crate::events::publish(conversation_id, crate::events::ConversationEvent::TurnError { message });
}

#[cfg(test)]
pub(super) fn remember_turn_error(conversation_id: i64, error: Option<String>) {
    with_runtime(conversation_id, |runtime| runtime.kept.error = error);
}

pub(crate) fn last_turn_error(conversation_id: i64) -> Option<String> {
    read_runtime(conversation_id, |runtime| runtime.kept.error.clone()).flatten()
}

/// Whether `conversation_id` still holds a turn lock and a stop counter,
/// for tests of what's freed.
#[cfg(test)]
pub(super) fn holds_lock_and_stops(conversation_id: i64) -> (bool, bool) {
    read_runtime(conversation_id, |runtime| (runtime.lock.is_some(), runtime.stops.is_some())).unwrap_or((false, false))
}

/// How many turns `conversation_id` has running or queued, for tests
/// that wait for a turn to queue.
#[cfg(test)]
pub(super) fn turns_in_flight(conversation_id: i64) -> usize {
    read_runtime(conversation_id, |runtime| runtime.turns_in_flight).unwrap_or(0)
}

/// Whether `conversation_id` has any turn state at all, for tests of
/// what's freed.
#[cfg(test)]
pub(super) fn has_turn_state(conversation_id: i64) -> bool {
    read_runtime(conversation_id, |_| ()).is_some()
}
