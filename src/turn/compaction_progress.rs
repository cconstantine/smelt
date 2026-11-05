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

/// How many compactions in a row may leave too little room under `limit`
/// before the conversation is judged not to fit. Past it the turn ends, even
/// if the request would still go out with a very short reply.
const MAX_AT_LIMIT: u32 = 3;

/// What a turn does about compacting before a model call.
#[derive(Debug, PartialEq)]
pub(super) enum CompactionStep {
    Go,
    Compact,
    /// The turn ends, for this reason.
    Stuck(String),
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
}

/// How near the window a request may come once compacting can't help.
fn limit(context_window: u32) -> u64 {
    u64::from(context_window).saturating_sub(u64::from(COMPACTION_SAFETY_BUFFER) + SHORT_REPLY_TOKENS)
}

impl CompactionProgress {
    fn sync(&mut self, context_window: u32) {
        if self.window != Some(context_window) {
            *self = Self { window: Some(context_window), ..Self::default() };
        }
    }

    /// What to do before a model call with a request of about `projected`
    /// tokens (`None`: not measured since the history last changed shape).
    pub(super) fn step(&mut self, model: &str, context_window: u32, projected: Option<u64>) -> CompactionStep {
        self.sync(context_window);
        if self.stuck {
            return CompactionStep::Stuck(self.why_stuck(model, context_window));
        }
        let trigger = if self.tight { limit(context_window) } else { compaction_ceiling(context_window) };
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

    /// The user wrote: they may try again. One result short of `stuck`, so a
    /// conversation that still doesn't fit costs them one summary, not three.
    pub(super) fn user_wrote(&mut self) {
        self.stuck = false;
        self.at_limit = self.at_limit.min(MAX_AT_LIMIT - 1);
    }

    /// Whether a request past the usual trigger has to keep inside the room
    /// left: the reply budget's floor assumes the trigger kept that room.
    pub(super) fn room_only(&self) -> bool {
        self.tight
    }

    fn why_stuck(&self, model: &str, context_window: u32) -> String {
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
