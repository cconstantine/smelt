//! The Claude subscription's five-hour usage gate (SME-523): every Claude
//! Code API call smelt makes waits here, between calls, while the
//! window's average pace would pass `LINE` by its reset.
//!
//! **The verdict** (`verdict`, pure): with `u` the window's utilization
//! and `e` the share of its five hours gone by, floored at `FLOOR` (one
//! burst early in a window would otherwise project wildly), the gate is
//! held while `u / e ≥ LINE`, and until the reset for a `rejected` status
//! or `u ≥ LINE`. With no new usage `u / e` only falls, so a hold's end is
//! computed, `start + 5 h × u / LINE`, not polled. No reading, no reset
//! time or a reset already passed is open: a missing reading never stops
//! Claude Code for good.
//!
//! **The reading** is one process-wide `five_hour` window from any Claude
//! Code provider (they share the server's one subscription): `usage::seen`
//! feeds each as the call streams, and `load` seeds it from the database at
//! startup. The newest `seen_at` wins, so a late, older reading is ignored.
//!
//! **Waiters** (a gated call each) queue only while the gate is held or
//! others queue: a call that finds it open and no one waiting passes at
//! once. Coming out of a hold they pass one per `RELEASE_SPACING`, warm
//! caches first (a pass under `WARM_FOR` ago: a cache read costs less of
//! the window than a full prefill, SME-346's idea), then oldest first,
//! each re-checking the latest reading as it goes. Warmth is decided at
//! each release, from the conversation's last pass then, not when its call
//! queued: a cache can go cold while its call waits (PR #353 review 1 M1).
//! And a waiter that has waited `COLD_LIMIT` ranks with the warm ones, so
//! a cold call isn't starved for the rest of the window by warm ones that
//! keep coming back. A waiter dropped (a Stop, a dropped turn) leaves the
//! queue at once.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::usage::Reading;

/// The share of the window the pace may reach by the reset.
pub const LINE: f64 = 0.95;
/// The `five_hour` window's length.
pub const WINDOW: Duration = Duration::hours(5);
/// The least share of the window the pace counts as gone by (SME-486's
/// first-seventh rule).
pub const FLOOR: f64 = 1.0 / 7.0;
/// How far apart waiters pass coming out of a hold.
pub const RELEASE_SPACING: StdDuration = StdDuration::from_secs(15);
/// How long after a conversation's last pass its cache counts as warm.
pub const WARM_FOR: StdDuration = StdDuration::from_secs(55 * 60);
/// How long a cold waiter waits before it ranks with the warm ones.
pub const COLD_LIMIT: StdDuration = StdDuration::from_secs(30 * 60);
/// The least a waiter behind others sleeps before it looks again unwoken,
/// so a gate with no spacing (a test's) doesn't spin.
const BEHIND_FLOOR: StdDuration = StdDuration::from_millis(100);

/// The `five_hour` window of a reading, as the gate keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct FiveHour {
    pub utilization: f64,
    pub status: String,
    pub resets_at: Option<DateTime<Utc>>,
    pub seen_at: DateTime<Utc>,
}

impl FiveHour {
    /// `reading`'s `five_hour` window, if it has one.
    pub fn from_reading(reading: &Reading) -> Option<Self> {
        reading.windows.iter().find(|w| w.name == "five_hour").map(|w| Self {
            utilization: w.utilization,
            status: w.status.clone(),
            resets_at: w.resets_at,
            seen_at: reading.seen_at,
        })
    }
}

/// Whether a call may go now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Open,
    Held { until: DateTime<Utc> },
}

/// The gate's verdict on `reading` at `now`.
///
/// Held while `u / max(e, FLOOR) ≥ LINE`. Past the floor that is exactly
/// `now < start + 5 h × u / LINE`, the release; under it, `u ≥ LINE ×
/// FLOOR` (and then the release lies past the floor, so `now` is before
/// it). Holding while `now < release`, rather than comparing `u / e`,
/// makes the release itself open, with no rounding to wait on.
pub fn verdict(reading: Option<&FiveHour>, now: DateTime<Utc>) -> Verdict {
    let Some(reading) = reading else {
        return Verdict::Open;
    };
    let Some(resets_at) = reading.resets_at.filter(|at| *at > now) else {
        return Verdict::Open;
    };
    let u = reading.utilization;
    if reading.status == "rejected" || u >= LINE {
        return Verdict::Held { until: resets_at };
    }
    if u < LINE * FLOOR {
        return Verdict::Open;
    }
    let start = resets_at - WINDOW;
    let release = start + Duration::milliseconds((WINDOW.num_milliseconds() as f64 * u / LINE) as i64);
    if now < release { Verdict::Held { until: release.min(resets_at) } } else { Verdict::Open }
}

/// One queued call.
struct Entry {
    id: u64,
    conversation_id: i64,
    since: Instant,
    /// The hold's end it was last told of, kept until it passes: its tabs
    /// show the hold until then (PR #353 review 1 L2).
    held: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct State {
    reading: Option<FiveHour>,
    waiters: Vec<Entry>,
    next_id: u64,
    last_release: Option<Instant>,
    /// Each conversation's last pass, while it's under `warm_for` old:
    /// older ones are pruned as passes are noted.
    last_pass: HashMap<i64, Instant>,
}

impl State {
    /// Notes `conversation_id`'s pass at `now`, pruning passes too old to
    /// count as warm.
    fn note_pass(&mut self, conversation_id: i64, now: Instant, warm_for: StdDuration) {
        self.last_pass.retain(|_, at| now.duration_since(*at) < warm_for);
        self.last_pass.insert(conversation_id, now);
    }
}

/// A gate: the process has one (`gate()`); tests make their own.
pub struct Gate {
    state: Mutex<State>,
    changed: Notify,
    spacing: StdDuration,
    warm_for: StdDuration,
    cold_limit: StdDuration,
}

impl Gate {
    pub fn new(spacing: StdDuration) -> Self {
        Self { state: Mutex::default(), changed: Notify::new(), spacing, warm_for: WARM_FOR, cold_limit: COLD_LIMIT }
    }

    /// A gate with its own `WARM_FOR` and `COLD_LIMIT`, for a test.
    #[cfg(test)]
    pub fn with_timing(spacing: StdDuration, warm_for: StdDuration, cold_limit: StdDuration) -> Self {
        Self { state: Mutex::default(), changed: Notify::new(), spacing, warm_for, cold_limit }
    }

    /// Queues an entry for `conversation_id` with no task behind it, for
    /// a test to hold a place in line; its id.
    #[cfg(test)]
    pub fn queue_for_tests(&self, conversation_id: i64) -> u64 {
        let mut state = self.state();
        let id = state.next_id;
        state.next_id += 1;
        state.waiters.push(Entry { id, conversation_id, since: Instant::now(), held: None });
        id
    }

    /// Takes entry `id` out of the line without waking anyone: a missed
    /// wake, for a test.
    #[cfg(test)]
    pub fn leave_quietly_for_tests(&self, id: u64) {
        self.state().waiters.retain(|e| e.id != id);
    }

    /// How many conversations' last passes it remembers.
    #[cfg(test)]
    pub fn remembered(&self) -> usize {
        self.state().last_pass.len()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every change is a few assignments under the lock: none is left
        // half-done by a panic elsewhere.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes `reading` if it's the newest yet.
    pub fn observe(&self, reading: FiveHour) {
        {
            let mut state = self.state();
            if state.reading.as_ref().is_some_and(|had| had.seen_at > reading.seen_at) {
                return;
            }
            state.reading = Some(reading);
        }
        self.changed.notify_waiters();
    }

    /// The verdict now.
    #[cfg(test)]
    pub fn verdict(&self) -> Verdict {
        verdict(self.state().reading.as_ref(), Utc::now())
    }

    /// The conversations waiting, in no particular order.
    #[cfg(test)]
    pub fn waiting(&self) -> Vec<i64> {
        self.state().waiters.iter().map(|e| e.conversation_id).collect()
    }

    /// The hold `conversation_id`'s waiting call shows: the gate's while
    /// it's held, and once it opens, the last one the call was told of,
    /// until it passes (as `wait` clears it only then). `None` for a call
    /// that queued behind others with the gate open, or none waiting.
    pub fn held_until(&self, conversation_id: i64) -> Option<DateTime<Utc>> {
        let state = self.state();
        let told = state.waiters.iter().filter(|e| e.conversation_id == conversation_id).map(|e| e.held).max()?;
        match verdict(state.reading.as_ref(), Utc::now()) {
            Verdict::Held { until } => Some(until),
            Verdict::Open => told,
        }
    }

    /// Waits until `conversation_id`'s call may go. `on_hold` is told the
    /// hold's end as it starts or changes, and `None` once it's over.
    pub async fn wait(&self, conversation_id: i64, mut on_hold: impl FnMut(Option<DateTime<Utc>>) + Send) {
        let Some(id) = self.enqueue(conversation_id) else {
            return;
        };
        let _queued = Queued { gate: self, id };
        let mut told: Option<DateTime<Utc>> = None;
        let mut was_next = false;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let step = self.step(id);
            // The order is decided at each look, so the waiter that was
            // next can find, its spacing up, that another has moved ahead
            // of it (a cache gone cold, a cold waiter aged): that one waits
            // on a change, so it's told of this one.
            if matches!(step, Step::Behind) && was_next {
                self.changed.notify_waiters();
            }
            was_next = matches!(step, Step::Spacing(_));
            let deadline = match step {
                Step::Held(until) => {
                    if told != Some(until) {
                        tracing::info!(conversation_id, release_at = %until, "a Claude Code call waits at the five-hour usage gate");
                        told = Some(until);
                        on_hold(Some(until));
                    }
                    Instant::now() + (until - Utc::now()).to_std().unwrap_or_default()
                }
                // Not on a wake alone: the order changes with time too, so
                // two waiters can each find the other first with no wake
                // to come (PR #353 review 2 L10). Within a spacing it looks
                // again; a release is never sooner than that anyway.
                Step::Behind => Instant::now() + self.spacing.max(BEHIND_FLOOR),
                Step::Spacing(at) => at,
                Step::Passed => {
                    if told.is_some() {
                        on_hold(None);
                    }
                    return;
                }
            };
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// Passes `conversation_id` if the gate is open and no one waits, as
    /// `wait` would at once; otherwise false, and nothing is queued. A
    /// cache re-seed's (SME-523): it never waits its turn.
    pub fn try_pass(&self, conversation_id: i64) -> bool {
        let mut state = self.state();
        if !state.waiters.is_empty() || verdict(state.reading.as_ref(), Utc::now()) != Verdict::Open {
            return false;
        }
        state.note_pass(conversation_id, Instant::now(), self.warm_for);
        true
    }

    /// Passes `conversation_id` at once if the gate is open and no one
    /// waits (`None`); otherwise queues it and returns its entry's id.
    fn enqueue(&self, conversation_id: i64) -> Option<u64> {
        let mut state = self.state();
        let now = Instant::now();
        if state.waiters.is_empty() && verdict(state.reading.as_ref(), Utc::now()) == Verdict::Open {
            // Not a release: the spacing only separates passes from the
            // queue, so the first out of a hold goes at once.
            state.note_pass(conversation_id, now, self.warm_for);
            return None;
        }
        let id = state.next_id;
        state.next_id += 1;
        state.waiters.push(Entry { id, conversation_id, since: now, held: None });
        Some(id)
    }

    /// One look at the queue for entry `id`: passes it (removing it) when
    /// the gate is open, it's first in line and the spacing is up. First
    /// in line is decided now: warm (its conversation passed under
    /// `warm_for` ago) or waiting `cold_limit` and more ranks ahead, then
    /// the oldest.
    fn step(&self, id: u64) -> Step {
        let mut state = self.state();
        if let Verdict::Held { until } = verdict(state.reading.as_ref(), Utc::now()) {
            if let Some(entry) = state.waiters.iter_mut().find(|e| e.id == id) {
                entry.held = Some(until);
            }
            return Step::Held(until);
        }
        let now = Instant::now();
        let ahead = |e: &Entry| {
            let warm = state.last_pass.get(&e.conversation_id).is_some_and(|at| now.duration_since(*at) < self.warm_for);
            warm || now.duration_since(e.since) >= self.cold_limit
        };
        let first = state.waiters.iter().min_by_key(|e| (!ahead(e), e.since, e.id)).map(|e| (e.id, e.conversation_id));
        let Some((first_id, conversation_id)) = first.filter(|(first_id, _)| *first_id == id) else {
            return Step::Behind;
        };
        if let Some(at) = state.last_release.map(|last| last + self.spacing).filter(|at| *at > now) {
            return Step::Spacing(at);
        }
        state.waiters.retain(|e| e.id != first_id);
        state.last_release = Some(now);
        state.note_pass(conversation_id, now, self.warm_for);
        Step::Passed
    }
}

/// What a waiter does next.
enum Step {
    Held(DateTime<Utc>),
    Behind,
    Spacing(Instant),
    Passed,
}

/// Takes a waiter's entry out of the queue however its wait ends, and
/// lets the next look.
struct Queued<'a> {
    gate: &'a Gate,
    id: u64,
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.gate.state().waiters.retain(|e| e.id != self.id);
        self.gate.changed.notify_waiters();
    }
}

/// The process's gate.
pub fn gate() -> &'static Gate {
    static GATE: LazyLock<Gate> = LazyLock::new(|| Gate::new(RELEASE_SPACING));
    &GATE
}

/// Feeds `reading`'s `five_hour` window, if any, to the process's gate.
pub fn observe(reading: &Reading) {
    if let Some(window) = FiveHour::from_reading(reading) {
        gate().observe(window);
    }
}

/// Seeds the process's gate from the newest recorded `five_hour` window of
/// any Claude Code provider. A failed read logs and leaves the gate open.
pub async fn load(pool: &sqlx::PgPool) {
    match crate::db::newest_claude_code_five_hour(pool).await {
        Ok(Some(row)) => gate().observe(FiveHour {
            utilization: row.utilization,
            status: row.status,
            resets_at: row.resets_at,
            seen_at: row.seen_at,
        }),
        Ok(None) => {}
        Err(e) => tracing::warn!("couldn't load the Claude subscription's five-hour window for the usage gate: {e}"),
    }
}

/// `Gate::try_pass` on the process's gate.
pub fn try_pass(conversation_id: i64) -> bool {
    gate().try_pass(conversation_id)
}

/// Waits at the process's gate for `conversation_id`'s Claude Code call,
/// telling the conversation's tabs about a hold (`UsageHold`) as it
/// starts or moves, and clearing it however the wait ends.
pub async fn wait(conversation_id: i64) {
    let mut shown = Shown { conversation_id, held: false };
    gate().wait(conversation_id, |until| shown.show(until)).await;
}

/// The hold a conversation's tabs were told of: cleared on drop, so a
/// Stop during a hold doesn't leave its line up.
struct Shown {
    conversation_id: i64,
    held: bool,
}

impl Shown {
    fn show(&mut self, until: Option<DateTime<Utc>>) {
        self.held = until.is_some();
        crate::events::publish(self.conversation_id, crate::events::ConversationEvent::UsageHold { until: until.map(|at| at.timestamp()) });
    }
}

impl Drop for Shown {
    fn drop(&mut self) {
        if self.held {
            self.show(None);
        }
    }
}

/// Clears the process's gate, for a test that set a reading on it.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *gate().state() = State::default();
    gate().changed.notify_waiters();
}
