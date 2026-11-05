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
        self.stuck.then(|| self.why_stuck(model, context_window))
    }

    /// Whether the conversation doesn't fit.
    pub(super) fn is_stuck(&self) -> bool {
        self.stuck
    }

    /// The user wrote: they may try again. One result short of `stuck`, so a
    /// conversation that still doesn't fit costs them one summary, not three.
    pub(super) fn user_wrote(&mut self) {
        // A send whose turn ended before it got that far keeps its try for the next.
        self.try_anyway |= self.stuck;
        self.stuck = false;
        self.refused_out = false;
        self.room_retried = false;
        self.at_limit = self.at_limit.min(MAX_AT_LIMIT - 1);
        self.refusal_compactions = self.refusal_compactions.min(MAX_REFUSAL_COMPACTIONS - 1);
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

    fn why_stuck(&self, model: &str, context_window: u32) -> String {
        if self.refused_out {
            return format!(
                "`{model}`'s server kept refusing this conversation as too big for its context window, however much \
                 smelt compacted it. Give the model a larger context on its server, or pick another model."
            );
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
mod tests {
    use super::*;

    const WINDOW: u32 = 100_000;
    const TRIGGER: u64 = 83_616;
    const LIMIT: u64 = 93_856;

    fn input(tokens: u64) -> anthropic::TokenUsage {
        anthropic::TokenUsage {
            input_tokens: tokens as i64,
            output_tokens: 50_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    fn after_compaction(progress: &mut CompactionProgress, tokens: u64) {
        progress.compacted(WINDOW);
        progress.measured(&input(tokens), WINDOW);
    }

    fn step(progress: &mut CompactionProgress, projected: u64) -> CompactionStep {
        progress.step("m", WINDOW, Some(projected))
    }

    #[test]
    fn test_the_usual_trigger_applies_until_something_is_learned() {
        let mut progress = CompactionProgress::default();
        assert_eq!(step(&mut progress, TRIGGER - 1), CompactionStep::Go);
        assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact);
        assert_eq!(progress.step("m", WINDOW, None), CompactionStep::Go, "nothing measured");
    }

    /// The value itself, in numbers: the integration tests would pass for
    /// anything from 617 to 5,616.
    #[test]
    fn test_a_compaction_has_to_leave_4096_tokens_under_the_trigger() {
        let mut progress = CompactionProgress::default();
        after_compaction(&mut progress, 79_520);
        assert_eq!(progress.tight_results, 0, "83,616 less 4,096 is enough");
        after_compaction(&mut progress, 79_521);
        assert_eq!(progress.tight_results, 1, "one token more isn't");
    }

    #[test]
    fn test_two_tight_results_in_a_row_move_the_trigger_to_the_limit() {
        let mut progress = CompactionProgress::default();
        after_compaction(&mut progress, TRIGGER - MIN_ROOM_TO_WORK + 1);
        assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact, "one isn't a pattern");
        after_compaction(&mut progress, TRIGGER - MIN_ROOM_TO_WORK + 1);
        assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Go);
        assert_eq!(step(&mut progress, LIMIT - 1), CompactionStep::Go);
        assert_eq!(step(&mut progress, LIMIT), CompactionStep::Compact);
        assert!(progress.room_only());
    }

    #[test]
    fn test_exactly_enough_room_isnt_tight_and_clears_what_was_learned() {
        let mut progress = CompactionProgress::default();
        for _ in 0..2 {
            after_compaction(&mut progress, TRIGGER);
        }
        assert!(progress.tight);
        after_compaction(&mut progress, TRIGGER - MIN_ROOM_TO_WORK);
        assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact);
        assert!(!progress.room_only());
        after_compaction(&mut progress, TRIGGER);
        assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact, "the count started again");
    }

    #[test]
    fn test_the_third_result_in_a_row_near_the_limit_is_stuck() {
        let mut progress = CompactionProgress::default();
        let near = LIMIT - SHORT_REPLY_TOKENS + 1;
        for _ in 0..2 {
            after_compaction(&mut progress, near);
            assert_eq!(step(&mut progress, LIMIT), CompactionStep::Compact);
        }
        after_compaction(&mut progress, near);
        let CompactionStep::Stuck(why) = step(&mut progress, 0) else {
            panic!("the conversation doesn't fit");
        };
        assert!(why.contains("(100,000 tokens) is too small") && why.contains("about 91,809 tokens"), "{why}");
    }

    #[test]
    fn test_a_result_with_room_under_the_limit_starts_that_count_again() {
        let mut progress = CompactionProgress::default();
        let near = LIMIT - SHORT_REPLY_TOKENS + 1;
        for _ in 0..2 {
            after_compaction(&mut progress, near);
        }
        after_compaction(&mut progress, LIMIT - SHORT_REPLY_TOKENS);
        for _ in 0..2 {
            after_compaction(&mut progress, near);
        }
        assert_eq!(step(&mut progress, 0), CompactionStep::Go);
    }

    #[test]
    fn test_the_user_writing_tries_again_one_short_of_stuck() {
        let mut progress = CompactionProgress::default();
        for _ in 0..3 {
            after_compaction(&mut progress, LIMIT);
        }
        assert!(matches!(step(&mut progress, 0), CompactionStep::Stuck(_)));
        progress.user_wrote();
        assert_eq!(step(&mut progress, LIMIT), CompactionStep::Compact);
        assert!(progress.tight, "what's known of the room stays");
        after_compaction(&mut progress, LIMIT);
        assert!(matches!(step(&mut progress, 0), CompactionStep::Stuck(_)));
    }

    #[test]
    fn test_a_new_window_forgets_everything() {
        let mut progress = CompactionProgress::default();
        for _ in 0..3 {
            after_compaction(&mut progress, LIMIT);
        }
        assert_eq!(progress.step("m", 500_000, Some(LIMIT)), CompactionStep::Go);
        assert!(!progress.room_only());
        // A compaction awaiting its measure, when a window is learned: the
        // measure is against nothing that was asked of it.
        progress.compacted(500_000);
        progress.measured(&input(LIMIT), 90_000);
        assert_eq!(progress.after, None);
    }

    #[test]
    fn test_nothing_is_learned_or_failed_against_a_guess() {
        let mut progress = CompactionProgress::default();
        progress.guessed(WINDOW, 40_000);
        for _ in 0..4 {
            progress.compacted(40_000);
            progress.measured(&input(39_000), 40_000);
        }
        assert_eq!(progress.step("m", 40_000, Some(39_000)), CompactionStep::Go, "the server says when it doesn't fit");
        assert!(!progress.room_only(), "and how much reply fits isn't known");
        // The next turn believes the real window again, and nothing carries.
        assert_eq!(step(&mut progress, TRIGGER - 1), CompactionStep::Go);
        assert_eq!(progress.after, None);
        assert_eq!(step(&mut progress, TRIGGER), CompactionStep::Compact);
    }

    #[test]
    fn test_a_guess_compacts_at_its_limit_not_its_trigger() {
        let mut progress = CompactionProgress::default();
        progress.guessed(WINDOW, 40_000);
        assert_eq!(progress.step("m", 40_000, Some(33_855)), CompactionStep::Go);
        assert_eq!(progress.step("m", 40_000, Some(33_856)), CompactionStep::Compact);
        progress.compacted(40_000);
        progress.measured(&input(10_000), 40_000);
        assert_eq!(progress.step("m", 40_000, Some(33_855)), CompactionStep::Go);
    }

    #[test]
    fn test_a_real_window_replacing_a_guess_starts_afresh() {
        let mut progress = CompactionProgress::default();
        progress.guessed(WINDOW, 40_000);
        assert_eq!(progress.step("m", 60_000, Some(43_616)), CompactionStep::Compact);
        assert_eq!(progress.guess, None);
    }

    #[test]
    fn test_the_fourth_refusal_in_a_row_ends_it_and_a_step_starts_the_count_again() {
        let mut progress = CompactionProgress::default();
        for _ in 0..3 {
            assert_eq!(progress.why_refused_out("m", WINDOW), None);
            progress.refusal_compacted(WINDOW);
        }
        progress.step_taken();
        assert_eq!(progress.why_refused_out("m", WINDOW), None);
        for _ in 0..3 {
            progress.refusal_compacted(WINDOW);
        }
        let why = progress.why_refused_out("m", WINDOW).expect("three in a row");
        assert!(why.contains("kept refusing") && !why.contains("100,000"), "{why}");
        assert!(matches!(step(&mut progress, 0), CompactionStep::Stuck(_)), "a wake doesn't start it over");
        progress.user_wrote();
        assert_eq!(step(&mut progress, 0), CompactionStep::Go);
        progress.refusal_compacted(WINDOW);
        assert!(progress.why_refused_out("m", WINDOW).is_some(), "a user's retry gets one more");
    }

    fn tight() -> CompactionProgress {
        let mut progress = CompactionProgress::default();
        for _ in 0..2 {
            after_compaction(&mut progress, 80_000);
        }
        assert!(progress.room_only());
        progress
    }

    #[test]
    fn test_a_reply_the_room_cut_off_compacts_next_whatever_its_size_then_once_only() {
        let mut progress = tight();
        assert!(progress.cut_for_room(Some(90_000), WINDOW));
        assert_eq!(step(&mut progress, 0), CompactionStep::Compact);
        assert_eq!(step(&mut progress, 0), CompactionStep::Go, "the one compaction");
        assert!(!progress.cut_for_room(Some(90_000), WINDOW), "a second cut in a row ends the turn");
        progress.reply_finished();
        assert!(progress.cut_for_room(Some(90_000), WINDOW), "a reply finished between");
        let mut progress = tight();
        assert!(progress.cut_for_room(Some(90_000), WINDOW));
        progress.user_wrote();
        assert!(progress.cut_for_room(Some(90_000), WINDOW), "the user's send too");
    }

    #[test]
    fn test_a_cut_is_only_answered_when_compacting_would_buy_room() {
        assert!(!tight().cut_for_room(None, WINDOW), "straight after a compaction");
        assert!(!tight().cut_for_room(Some(82_048), WINDOW), "exactly a short reply's worth isn't more");
        assert!(tight().cut_for_room(Some(82_049), WINDOW));
        let mut untight = CompactionProgress::default();
        after_compaction(&mut untight, 10_000);
        assert!(!untight.cut_for_room(Some(90_000), WINDOW), "the usual trigger kept the reply its room");
        let mut guessing = tight();
        guessing.guessed(WINDOW, 40_000);
        assert!(!guessing.cut_for_room(Some(39_000), 40_000));
        assert!(!tight().cut_for_room(Some(90_000), 500_000), "a window learned since");
    }

    #[test]
    fn test_the_floor_is_stuck_and_names_what_was_measured_not_the_estimate() {
        let mut progress = tight();
        after_compaction(&mut progress, 96_000);
        let FloorAnswer::NoRoom(why) = progress.floor_fired(99_000, "m", WINDOW) else {
            panic!("no room by either number");
        };
        assert!(why.contains("(100,000 tokens) is too small") && why.contains("about 96,000 tokens"), "{why}");
        assert!(progress.is_stuck());
        assert_eq!(progress.why_if_stuck("m", WINDOW).as_deref(), Some(why.as_str()));
        progress.user_wrote();
        assert_eq!(progress.why_if_stuck("m", WINDOW), None);
    }

    #[test]
    fn test_what_was_measured_outweighs_the_estimate_when_it_shows_room() {
        let mut progress = tight();
        assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::Believe(80_000));
        assert!(!progress.is_stuck());
        // Exactly the buffer and the floor, then one token over.
        after_compaction(&mut progress, 94_880);
        assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::Believe(94_880));
        after_compaction(&mut progress, 94_881);
        assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)));
    }

    #[test]
    fn test_with_nothing_measured_the_estimate_is_the_number_to_name() {
        let mut progress = CompactionProgress::default();
        let FloorAnswer::NoRoom(why) = progress.floor_fired(99_000, "m", WINDOW) else {
            panic!("no room");
        };
        assert!(why.contains("about 99,000 tokens"), "{why}");
    }

    #[test]
    fn test_the_users_send_after_stuck_lets_one_request_through() {
        let mut progress = tight();
        after_compaction(&mut progress, 96_000);
        assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)));
        progress.user_wrote();
        assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::TryAnyway);
        assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)), "one");
        progress.user_wrote();
        progress.step_taken();
        assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)), "a step taken uses it up");
    }

    #[test]
    fn test_a_send_whose_turn_never_got_to_the_floor_keeps_its_try() {
        let mut progress = tight();
        after_compaction(&mut progress, 96_000);
        assert!(matches!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::NoRoom(_)));
        progress.user_wrote();
        progress.user_wrote();
        assert_eq!(progress.floor_fired(99_000, "m", WINDOW), FloorAnswer::TryAnyway);
    }

    #[test]
    fn test_only_the_first_reply_after_a_compaction_measures_it() {
        let mut progress = CompactionProgress::default();
        after_compaction(&mut progress, 10_000);
        progress.measured(&input(90_000), WINDOW);
        assert_eq!(progress.after, Some(10_000), "later replies grow with the turn's own work");
    }

    #[test]
    fn test_a_server_reporting_no_usage_measures_nothing() {
        let mut progress = CompactionProgress::default();
        after_compaction(&mut progress, 0);
        assert_eq!(progress.after, None);
        progress.measured(&input(90_000), WINDOW);
        assert_eq!(progress.after, Some(90_000), "the compaction was still waiting for a measurement");
    }

    #[test]
    fn test_cache_tokens_are_input_and_output_isnt() {
        let mut progress = CompactionProgress::default();
        progress.compacted(WINDOW);
        progress.measured(
            &anthropic::TokenUsage {
                input_tokens: 10,
                output_tokens: 90_000,
                cache_creation_input_tokens: 20,
                cache_read_input_tokens: 30,
            },
            WINDOW,
        );
        assert_eq!(progress.after, Some(60));
    }
}
