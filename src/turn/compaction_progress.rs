//! Whether a turn's compactions are getting anywhere (SME-386 F10). A turn
//! compacts whenever a request nears the window, as often as it needs to:
//! a long ask reading big outputs can compact after nearly every step and
//! still finish. What it can't do is keep compacting for nothing, which it
//! did when what compaction can't shrink (the system prompt and the tool
//! definitions) sits near or over the trigger: 24 summaries in 6 minutes,
//! one step between each, and no word to the user why.
//!
//! The state, one per turn:
//! - a compaction ran: `before` holds the size it answered (a second one
//!   before any reply replaces it);
//! - the next reply measures what the compaction left: `before` is taken,
//!   `after` set, and `low_yield` counts up or starts again. A server that
//!   reports no usage measures nothing, and nothing changes;
//! - the turn ends, however it does: the state goes with it, so the next
//!   turn gets its own tries. A failed summary or a Stop ends the turn too.

use super::*;

/// What a compaction has to free for it to have got anywhere: what its own
/// summary can take back (`SUMMARY_MAX_TOKENS`). Any compaction leaving this
/// much room under the trigger clears it whatever the turn does next, since
/// it frees at least the room it leaves.
const MIN_COMPACTION_YIELD: u64 = SUMMARY_MAX_TOKENS as u64;

/// How many compactions in a row may free less than that. One is no loop:
/// the last step may have brought little. Three is a turn whose window has
/// no room to work in, and a summary call paid for every few small steps.
const MAX_LOW_YIELD_COMPACTIONS: u32 = 3;

/// One turn's record of what its compactions freed.
#[cfg(feature = "server")]
#[derive(Debug, Default)]
pub(super) struct CompactionProgress {
    /// The size the last compaction answered, until a reply measures it.
    before: Option<u64>,
    /// The input a reply measured just after the last compaction: the
    /// smallest a compaction can make this conversation's request.
    after: Option<u64>,
    /// Compactions in a row that freed less than `MIN_COMPACTION_YIELD`.
    low_yield: u32,
}

#[cfg(feature = "server")]
impl CompactionProgress {
    /// A compaction just answered a request of about `before` tokens.
    pub(super) fn compacted(&mut self, before: u64) {
        self.before = Some(before);
    }

    /// A reply's `usage`. Its input alone: what it wrote is new work, not
    /// something the compaction failed to free.
    pub(super) fn measured(&mut self, usage: &anthropic::TokenUsage) {
        let input = usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.cache_read_input_tokens)
            .max(0) as u64;
        if input == 0 {
            return;
        }
        let Some(before) = self.before.take() else {
            return;
        };
        self.after = Some(input);
        self.low_yield = if before.saturating_sub(input) < MIN_COMPACTION_YIELD { self.low_yield + 1 } else { 0 };
    }

    /// Why compacting again wouldn't help, for a turn about to: a request
    /// already over the trigger straight after a compaction will trigger it
    /// again at once, and a run of low yields says the same more slowly.
    pub(super) fn why_stuck(&self, model: &str, context_window: u32) -> Option<String> {
        let after = self.after?;
        let ceiling = compaction_ceiling(context_window);
        (after >= ceiling || self.low_yield >= MAX_LOW_YIELD_COMPACTIONS).then(|| {
            format!(
                "`{model}`'s context window ({} tokens) is too small for this conversation. smelt compacts once a \
                 request leaves less than {} tokens free for the reply, and even compacted a request takes about {} \
                 tokens, most of it the system prompt and tools, which compacting can't shrink. Compacting again \
                 wouldn't get this turn anywhere, so it stops here. Give the model a larger context window on its \
                 server and on its provider's page, or pick another model.",
                super::overflow::grouped(u64::from(context_window)),
                super::overflow::grouped(u64::from(MIN_REPLY_TOKENS.max(COMPACTION_SAFETY_BUFFER))),
                super::overflow::grouped(after),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tokens: i64) -> anthropic::TokenUsage {
        anthropic::TokenUsage {
            input_tokens: tokens,
            output_tokens: 50_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    fn stuck(progress: &CompactionProgress) -> bool {
        progress.why_stuck("m", 100_000).is_some()
    }

    #[test]
    fn test_no_compaction_yet_is_never_stuck() {
        let mut progress = CompactionProgress::default();
        progress.measured(&input(99_000));
        assert!(!stuck(&progress), "only what a compaction left says anything");
    }

    #[test]
    fn test_a_compacted_request_over_the_trigger_is_stuck_at_once() {
        let mut progress = CompactionProgress::default();
        progress.compacted(200_000);
        progress.measured(&input(83_616));
        assert!(stuck(&progress), "it freed plenty and it's still at the trigger");
        let mut progress = CompactionProgress::default();
        progress.compacted(200_000);
        progress.measured(&input(83_615));
        assert!(!stuck(&progress), "one token under it");
    }

    #[test]
    fn test_the_third_low_yield_in_a_row_is_stuck() {
        let mut progress = CompactionProgress::default();
        for round in 1..=3 {
            assert!(!stuck(&progress), "before round {round}");
            progress.compacted(70_000);
            progress.measured(&input(70_000 - i64::from(SUMMARY_MAX_TOKENS) + 1));
        }
        assert!(stuck(&progress));
    }

    #[test]
    fn test_freeing_exactly_the_yield_counts_as_real_room_and_starts_the_count_again() {
        let mut progress = CompactionProgress::default();
        for _ in 0..2 {
            progress.compacted(70_000);
            progress.measured(&input(69_000));
        }
        progress.compacted(70_000);
        progress.measured(&input(70_000 - i64::from(SUMMARY_MAX_TOKENS)));
        for _ in 0..2 {
            progress.compacted(70_000);
            progress.measured(&input(69_000));
        }
        assert!(!stuck(&progress), "two in a row, after a real one");
    }

    #[test]
    fn test_only_the_first_reply_after_a_compaction_measures_it() {
        let mut progress = CompactionProgress::default();
        progress.compacted(70_000);
        progress.measured(&input(10_000));
        progress.measured(&input(90_000));
        assert!(!stuck(&progress), "later replies grow with the turn's own work");
    }

    #[test]
    fn test_a_server_reporting_no_usage_measures_nothing() {
        let mut progress = CompactionProgress::default();
        progress.compacted(70_000);
        progress.measured(&input(0));
        assert!(!stuck(&progress));
        progress.measured(&input(90_000));
        assert!(stuck(&progress), "the compaction is still waiting for a measurement");
    }

    #[test]
    fn test_cache_tokens_are_input_and_output_isnt() {
        let mut progress = CompactionProgress::default();
        progress.compacted(200_000);
        progress.measured(&anthropic::TokenUsage {
            input_tokens: 10,
            output_tokens: 90_000,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 83_590,
        });
        assert!(stuck(&progress), "83,620 of input, cache included, is over the 83,616 trigger");
        let mut progress = CompactionProgress::default();
        progress.compacted(200_000);
        progress.measured(&input(10_000));
        assert!(!stuck(&progress), "what the reply wrote (50,000) isn't what the compaction left");
    }

    #[test]
    fn test_a_window_learned_later_is_what_it_is_judged_against() {
        let mut progress = CompactionProgress::default();
        progress.compacted(70_000);
        progress.measured(&input(30_000));
        assert!(progress.why_stuck("m", 100_000).is_none());
        let why = progress.why_stuck("m", 40_000).expect("30,000 is over the 23,616 trigger");
        assert!(why.contains("(40,000 tokens)") && why.contains("16,384") && why.contains("30,000"), "{why}");
    }
}
