//! Whether compacting is getting a conversation anywhere (SME-386 F10). A
//! turn compacts whenever a request nears the window, as often as it needs
//! to: a long ask reading big outputs can compact after nearly every step
//! and still finish. What it can't do is keep compacting for nothing, which
//! it did when what compaction can't shrink (the system prompt and the tool
//! definitions) sits near or over the trigger: 24 summaries in 6 minutes,
//! one step between each, and no word to the user why.
//!
//! So each compaction is judged by the room it leaves: the input the next
//! reply measures, against the trigger. (Not by what it freed, which is
//! only the size of the step before it.) Two tight results in a row mean
//! compacting can't help, and it's skipped from then on until the request
//! nears the window itself (`limit`), where it has real history to free.
//! Those requests ask for no more reply than fits, so nothing goes out that
//! the provider has to refuse. A compaction that can't get clear of even
//! that limit, three times running, means the conversation doesn't fit, and
//! the turn ends saying so.
//!
//! The record is the conversation's, in its `ConversationRuntime`, since a
//! turn that ended is followed by wakes that would learn it all over again:
//! - a compaction ran (`compacted`): `awaiting` until a reply measures it;
//! - a reply measures it (`measured`): the counts move, and `tight` or
//!   `stuck` may be set. A server reporting no usage measures nothing;
//! - a compaction that leaves room to work in clears `tight` again;
//! - the window changes (`sync`, on any use, a provider page's edit or a
//!   window learned mid-turn alike): all of it goes, being about the old one;
//! - the user's send or answer (`user_wrote`): `stuck` goes, so they can try
//!   again, one result short of it. `tight` stays: it only delays a compaction;
//! - a refusal naming no window has the turn guess one, for that turn alone
//!   (`guessed`): nothing it shows is about the real window, so nothing is
//!   learned or failed against it (review M2), the usual trigger gives way
//!   to `limit`, and it's dropped at the next turn;
//! - the server refuses a request and a compaction answers it
//!   (`refusal_compacted`), no usage needed (review L2): three in a row, with
//!   no step the server took between, set `stuck` too;
//! - a reply the room cut off, past the usual trigger, with history a
//!   compaction would free (`cut_for_room`, round 2 M1): the next step
//!   compacts whatever its size, and the model is called again. Once: a second
//!   cut in a row ends the turn as any cut-off reply does, and a reply that
//!   wasn't cut (`reply_finished`) or the user's send allows it again;
//! - a compacted request is under the floor (`floor_fired`, review M-A): what
//!   a reply measured after a compaction is believed over the estimate if it
//!   shows room. If not, `stuck`, unless the user has written since it last
//!   was (`try_anyway`), which lets one request through: only the server's
//!   answer can show anything new, so the user always has a way out;
//! - the server refuses a request whose budget that measured size gave
//!   (`refused`, final review M-1): the size is out of date, and the larger
//!   of it and the refusal's count is what's believed from then on;
//! - compaction after every step — one step between each summary — is the
//!   loop SME-386 F10 survived: counted (`thrash`), the model is told at
//!   `NOTICE_AT`, and at `END_AT` the turn ends and wakes stop until the
//!   user writes, the window changes, or a message queues (SME-414);
//! - a restart or a delete: all of it goes, and the next compaction learns it.

use super::*;

/// How much room under the usual trigger a compaction has to leave to count
/// as having helped. Less is a compaction every step or two. Generous,
/// because too little only moves the compaction out to `limit`.
const MIN_ROOM_TO_WORK: u64 = 4096;

/// How many tight results in a row show compacting can't help. One doesn't:
/// a compaction's quotes of unanswered messages inflate only the request
/// right after it (`cap_unanswered` keeps those small too).
const TIGHT_RESULTS: u32 = 2;

/// The room left for a reply at `limit`, on top of the buffer.
const SHORT_REPLY_TOKENS: u64 = 2048;

/// The least room worth sending a request for: under it a compacted request
/// can't do anything but be cut off, which is no answer to the user.
pub(super) const MIN_USEFUL_REPLY: u32 = 1024;

/// How many compactions in a row may leave too little room under `limit`
/// before the conversation is judged not to fit. Past it the turn ends, even
/// if the request would still go out with a very short reply.
const MAX_AT_LIMIT: u32 = 3;

/// How many compactions answering a refusal may come in a row, with no step
/// the server took between them.
const MAX_REFUSAL_COMPACTIONS: u32 = 3;

/// How many single-step compactions in a row — one step between each — tell
/// the model what it's in for: every step bigger than the room a compaction
/// leaves. 9, under F11's real 11 compactions over 9 steps, so the loop is
/// named while it can still bend, and above the 8-compaction guard test
/// (Decided (Driver) D1, 2026-11-10: under the user's answer — end the turn,
/// no clamp — a notice fired only at the end would never reach the model).
pub(super) const NOTICE_AT: u32 = 9;

/// How many single-step compactions in a row end the turn rather than keep
/// paying for summaries (the user's answer: end with a clear notice;
/// Decided (Driver) D1).
pub(super) const END_AT: u32 = 12;

/// What a turn does about compacting before a model call.
#[derive(Debug, PartialEq)]
pub(super) enum CompactionStep {
    Go,
    Compact,
    /// The turn ends, for this reason.
    Stuck(String),
}

/// What to do with a compacted request the estimate leaves under the floor.
#[derive(Debug, PartialEq)]
pub(super) enum FloorAnswer {
    /// A reply measured this size after a compaction, and it leaves room.
    Believe(u64),
    /// The user wrote since this conversation was last judged not to fit.
    TryAnyway,
    /// It doesn't fit, for this reason.
    NoRoom(String),
}

/// A conversation's record of what its compactions left.
#[derive(Clone, Debug, Default)]
pub(super) struct CompactionProgress {
    /// The window all this was learned of.
    window: Option<u32>,
    /// A compaction ran and no reply has measured it yet.
    awaiting: bool,
    /// The input a reply last measured just after a compaction.
    after: Option<u64>,
    /// Tight results in a row.
    tight_results: u32,
    /// Compacting can't help: wait for `limit`.
    tight: bool,
    /// Results in a row leaving too little room under `limit`.
    at_limit: u32,
    /// The conversation doesn't fit: end the turn.
    stuck: bool,
    /// This turn's guess at the window, in place of `window`.
    guess: Option<u32>,
    /// Compactions answering a refusal in a row.
    refusal_compactions: u32,
    /// `stuck` because of those, so no size or window is known to name.
    refused_out: bool,
    /// The next step compacts, for the reply the room just cut off.
    compact_for_room: bool,
    /// That's been tried, and no reply has finished since.
    room_retried: bool,
    /// The user wrote after `stuck`: one request goes, whatever the floor says.
    try_anyway: bool,
    /// The request out now has the budget a measured `after` gave it.
    believing: bool,
    /// `stuck` because the system prompt and tools take about this much,
    /// which leaves this window no room for a reply.
    too_small: Option<u64>,
    /// Accepted model steps since the last compaction.
    steps_since_compaction: u32,
    /// Compactions in a row with exactly one step between them: each step
    /// is spent before the model can use the room the last left (SME-414).
    thrash: u32,
    /// The model was told at `NOTICE_AT` (`take_thrash_notice` reads it).
    thrash_noticed: bool,
    /// `thrash` reached `END_AT`: turns end and wakes stop, until the user
    /// writes, the window changes, or a message queues (`clear_thrash`).
    thrashing: bool,
}

/// How near the window a request may come once compacting can't help.
fn limit(context_window: u32) -> u64 {
    u64::from(context_window).saturating_sub(u64::from(COMPACTION_SAFETY_BUFFER) + SHORT_REPLY_TOKENS)
}

impl CompactionProgress {
    fn sync(&mut self, context_window: u32) {
        if self.guess == Some(context_window) {
            return;
        }
        if self.window != Some(context_window) {
            *self = Self { window: Some(context_window), ..Self::default() };
        } else if self.guess.take().is_some() {
            // The turn that guessed is over, and what it saw was against the guess.
            self.forget_measures();
        }
    }

    fn forget_measures(&mut self) {
        (self.after, self.awaiting, self.tight, self.tight_results, self.at_limit) = (None, false, false, 0, 0);
        // Against a guess the loop can't be measured; its armed stop
        // survives, like `refused_out` (SME-414).
        (self.thrash, self.thrash_noticed) = (0, false);
    }

    /// A refusal naming no window has the turn believe `guess` instead of
    /// `believed`, for this turn alone.
    pub(super) fn guessed(&mut self, believed: u32, guess: u32) {
        self.sync(believed);
        self.guess = Some(guess);
        self.forget_measures();
    }

    /// A compaction just answered a refusal.
    pub(super) fn refusal_compacted(&mut self, context_window: u32) {
        self.compacted(context_window);
        self.refusal_compactions += 1;
    }

    /// The server took a request as it was sent.
    pub(super) fn step_taken(&mut self) {
        self.refusal_compactions = 0;
        self.try_anyway = false;
        self.believing = false;
        self.steps_since_compaction += 1;
    }

    /// The server took the retry after a refusal: the try and what was
    /// believed are spent, but the refusal count holds, or a server refusing
    /// every step would compact at every step (review L2).
    pub(super) fn retry_taken(&mut self) {
        self.try_anyway = false;
        self.believing = false;
    }

    /// The request out died in something other than a refusal: the size
    /// believed for it is spent, and must not go on to make some later
    /// refusal read as that size's disproof (SME-423). Nothing else is
    /// proved — `after`, `awaiting` and `tight` stand.
    pub(super) fn request_failed(&mut self) {
        self.believing = false;
    }

    /// The server refused a request, saying it held `input` tokens if it
    /// said. One whose budget came from a measured `after` shows that size is
    /// out of date (the system prompt or tools grew since), so it's believed
    /// no longer (SME-386 final review M-1): the floor then refuses, and the
    /// user's next send tries with a short reply.
    pub(super) fn refused(&mut self, input: Option<u64>) {
        if std::mem::take(&mut self.believing) {
            self.after = input.map(|input| self.after.map_or(input, |after| after.max(input)));
        }
    }

    /// The system prompt and tools, about `alone` tokens, leave this window
    /// no room for a reply: wakes say so without calling, until the user
    /// writes or the window changes (final review, the property test).
    pub(super) fn too_small(&mut self, alone: u64, context_window: u32) {
        self.sync(context_window);
        self.too_small = Some(alone);
        self.stuck = true;
    }

    /// The server refused a request: the reason, if another compaction
    /// wouldn't be worth asking for.
    pub(super) fn why_refused_out(&mut self, model: &str, context_window: u32) -> Option<String> {
        self.sync(context_window);
        if self.refusal_compactions < MAX_REFUSAL_COMPACTIONS {
            return None;
        }
        self.stuck = true;
        self.refused_out = true;
        Some(self.why_stuck(model, context_window))
    }

    /// What to do before a model call with a request of about `projected`
    /// tokens (`None`: not measured since the history last changed shape).
    pub(super) fn step(&mut self, model: &str, context_window: u32, projected: Option<u64>) -> CompactionStep {
        self.sync(context_window);
        if self.stuck {
            return CompactionStep::Stuck(self.why_stuck(model, context_window));
        }
        // The loop counted out (SME-414): an armed wake stops before the
        // call, on wording that names no numbers so `already_said` dedupes
        // its repeats; the count reaching `END_AT` arms it and ends the
        // turn. Skipped under a guess, as `tight` is.
        if self.thrashing {
            return CompactionStep::Stuck(Self::why_armed(model));
        }
        if self.guess.is_none() && self.thrash >= END_AT {
            self.thrashing = true;
            return CompactionStep::Stuck(Self::why_thrashed(model));
        }
        if std::mem::take(&mut self.compact_for_room) {
            return CompactionStep::Compact;
        }
        // Against a guess, a compaction that can't get under it is no sign
        // of anything: the server says when a request really doesn't fit.
        if self.guess.is_some() && self.after.is_some_and(|after| after >= limit(context_window)) {
            return CompactionStep::Go;
        }
        let trigger = if self.tight || self.guess.is_some() { limit(context_window) } else { compaction_ceiling(context_window) };
        if projected.is_some_and(|projected| projected >= trigger) {
            CompactionStep::Compact
        } else {
            CompactionStep::Go
        }
    }

    /// A compaction just ran.
    pub(super) fn compacted(&mut self, context_window: u32) {
        self.sync(context_window);
        self.awaiting = true;
        // The loop's count (SME-414): exactly one step between two
        // compactions is the shape. Two worked steps mean the room was
        // used, and the run starts over; none — refusal-answering and
        // recovery chains — is `MAX_REFUSAL_COMPACTIONS`' job, not this
        // one. Nothing is counted against a guess.
        if self.guess.is_none() {
            match self.steps_since_compaction {
                1 => {
                    self.thrash += 1;
                    self.thrash_noticed |= self.thrash == NOTICE_AT;
                }
                0 => {}
                _ => self.thrash = 0,
            }
        }
        self.steps_since_compaction = 0;
    }

    /// A reply's `usage`. Its input alone: what it wrote is new work, not
    /// something the compaction failed to free. Only the first reply after a
    /// compaction measures it: later ones grow with the turn's own work.
    pub(super) fn measured(&mut self, usage: &anthropic::TokenUsage, context_window: u32) {
        self.sync(context_window);
        let input = usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.cache_read_input_tokens)
            .max(0) as u64;
        if !self.awaiting || input == 0 {
            return;
        }
        self.awaiting = false;
        self.after = Some(input);
        if self.guess.is_some() {
            return;
        }
        if input + MIN_ROOM_TO_WORK <= compaction_ceiling(context_window) {
            self.tight_results = 0;
            self.tight = false;
            self.at_limit = 0;
            return;
        }
        self.tight_results += 1;
        self.tight |= self.tight_results >= TIGHT_RESULTS;
        if input + SHORT_REPLY_TOKENS > limit(context_window) {
            self.at_limit += 1;
            self.stuck |= self.at_limit >= MAX_AT_LIMIT;
        } else {
            self.at_limit = 0;
        }
    }

    /// A reply held to the room left was cut off at it, on a request of
    /// about `sized` tokens (`None`: straight after a compaction, so there's
    /// nothing more to free). Whether to compact and call the model again:
    /// compacting has to buy room, at least what `limit` keeps for a reply.
    pub(super) fn cut_for_room(&mut self, sized: Option<u64>, context_window: u32) -> bool {
        self.sync(context_window);
        let frees_room = sized.zip(self.after).is_some_and(|(sized, after)| after + SHORT_REPLY_TOKENS < sized);
        if !self.room_only() || self.room_retried || !frees_room {
            return false;
        }
        self.compact_for_room = true;
        self.room_retried = true;
        true
    }

    /// A reply finished rather than being cut off.
    pub(super) fn reply_finished(&mut self) {
        self.room_retried = false;
    }

    /// A request of about `size` tokens, compacted just now, leaves no useful
    /// room by the estimate.
    pub(super) fn floor_fired(&mut self, size: u64, model: &str, context_window: u32) -> FloorAnswer {
        self.sync(context_window);
        let fits = |after: &u64| after + u64::from(COMPACTION_SAFETY_BUFFER) + u64::from(MIN_USEFUL_REPLY) <= u64::from(context_window);
        if let Some(after) = self.after.filter(fits) {
            self.believing = true;
            return FloorAnswer::Believe(after);
        }
        if std::mem::take(&mut self.try_anyway) {
            return FloorAnswer::TryAnyway;
        }
        // What a reply measured, if any, is the better number to name.
        self.after.get_or_insert(size);
        self.stuck = true;
        FloorAnswer::NoRoom(self.why_stuck(model, context_window))
    }

    /// The reason, if the conversation doesn't fit: for a reply cut off on
    /// the result that shows it, whose turn never gets to `step`.
    pub(super) fn why_if_stuck(&self, model: &str, context_window: u32) -> Option<String> {
        (self.stuck || self.thrashing).then(|| self.why_stuck(model, context_window))
    }

    /// Whether the conversation doesn't fit.
    pub(super) fn is_stuck(&self) -> bool {
        self.stuck || self.thrashing
    }

    /// Whether the loop's stop is armed, for tests that watch it lift.
    #[cfg(test)]
    pub(super) fn is_thrashing(&self) -> bool {
        self.thrashing
    }

    /// The loop's notice to the model, once (SME-414): fired by the
    /// compaction that counted `NOTICE_AT`, drained by the turn right after
    /// it, which saves it grey-marked (SME-357) so a later compaction
    /// quotes it as the model's typed words.
    pub(super) fn take_thrash_notice(&mut self) -> Option<String> {
        std::mem::take(&mut self.thrash_noticed).then(|| {
            format!(
                "smelt notice: this turn has compacted {} times in a row — each step is bigger than the room a \
                 compaction leaves. Keep the next steps small: read files with offset/limit, one at a time, and \
                 don't re-read whole files. Smelt stops the turn if the loop keeps up.",
                self.thrash
            )
        })
    }

    /// A message queued for this conversation lifts the loop's stop
    /// (SME-414 D2): an agent's operator is its parent, and an armed child
    /// must still read it. `stuck` stays — the message changes no sizes.
    pub(super) fn clear_thrash(&mut self) {
        self.thrash = 0;
        self.thrash_noticed = false;
        self.thrashing = false;
    }

    /// The user wrote: they may try again. One result short of `stuck`, so a
    /// conversation that still doesn't fit costs them one summary, not three.
    pub(super) fn user_wrote(&mut self) {
        // A send whose turn ended before it got that far keeps its try for the next.
        self.try_anyway |= self.stuck;
        self.stuck = false;
        self.refused_out = false;
        self.too_small = None;
        self.room_retried = false;
        self.compact_for_room = false;
        self.at_limit = self.at_limit.min(MAX_AT_LIMIT - 1);
        self.refusal_compactions = self.refusal_compactions.min(MAX_REFUSAL_COMPACTIONS - 1);
        // The loop, fully: thrash isn't "doesn't fit", and the user's send
        // gets fresh runway (SME-414; the escape property needs it).
        self.clear_thrash();
    }

    /// `room_only`, for a window that may have just changed.
    pub(super) fn room_only_at(&mut self, context_window: u32) -> bool {
        self.sync(context_window);
        self.room_only()
    }

    /// Whether a request past the usual trigger has to keep inside the room
    /// left: the reply budget's floor assumes the trigger kept that room.
    pub(super) fn room_only(&self) -> bool {
        self.tight && self.guess.is_none()
    }

    /// Why the loop ended the turn: the steps, not the fit. Names `END_AT`,
    /// which only ever changes with the code.
    fn why_thrashed(model: &str) -> String {
        format!(
            "`{model}` was asked for work whose every step was bigger than the room a compaction left: {END_AT} \
             compactions in a row, one step between each. The turn ends rather than keep paying for summaries — the \
             conversation still fits; it is the steps that don't. Ask for the work in smaller pieces (read with \
             offset/limit, one file at a time), give the model a larger context window, or start a fresh conversation."
        )
    }

    /// Why an armed wake stops before the call. Number-free on purpose:
    /// `already_said` dedupes on the identical text, and any number here
    /// would change and re-tell it.
    fn why_armed(model: &str) -> String {
        format!(
            "`{model}`'s turn was stopped for compacting after every step and will not try again until someone \
             writes to this conversation. Break the work into smaller steps or pick a larger window."
        )
    }

    fn why_stuck(&self, model: &str, context_window: u32) -> String {
        if let Some(alone) = self.too_small {
            return super::overflow::too_small(model, context_window, alone);
        }
        if self.refused_out {
            return format!(
                "`{model}`'s server kept refusing this conversation as too big for its context window, however much \
                 smelt compacted it. Give the model a larger context window on its server and on its provider's page, or pick another model."
            );
        }
        if self.thrashing {
            // The conversation fits: it is the steps that don't. Named as
            // such, and number-free like the armed wake's wording.
            return Self::why_armed(model);
        }
        let size = self.after.map_or_else(String::new, |after| format!(" about {} tokens,", super::overflow::grouped(after)));
        format!(
            "`{model}`'s context window ({} tokens) is too small for this conversation: even compacted, a request takes{size} \
             which leaves too little room for a reply, and compacting again wouldn't free any more. Give the model a \
             larger context window on its server and on its provider's page, or pick another model.",
            super::overflow::grouped(u64::from(context_window)),
        )
    }
}

#[cfg(test)]
mod tests;
