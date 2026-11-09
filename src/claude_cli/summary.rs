//! What a compaction does with the summary its model call returned
//! (SME-431, PR #291's reviews; the HTTP kinds' decisions since
//! SME-443). The CLI's summary shares its cap with thinking that can't
//! be turned off, so it can come back empty or cut off; saved, it would
//! replace the history with nothing or a stub. An HTTP kind's summary
//! cut off at its (room-derived, SME-443) budget is continued once
//! instead of saved mid-sentence; what the exchange yields saves whole,
//! or saves marked where it still stopped — SME-393's honesty.

use crate::anthropic::stream::{is_cut_off, ModelError};
use crate::providers::ProviderKind;

/// The summary, or why it isn't saved over the history. Only the Claude
/// Code kind refuses anything here (the user's Q1, SME-431): an HTTP
/// kind's cut-off summary is continued once rather than saved
/// mid-sentence — `http_first` and `http_continued` below are that
/// decision, and what they hand back saves as it always did.
pub fn checked(kind: ProviderKind, summary: String, stop_reason: &str) -> Result<String, ModelError> {
    if kind != ProviderKind::ClaudeCli {
        return Ok(summary);
    }
    // Stopped by the window, not its cap: the compaction's overflow arm
    // asks once more with half the transcript, where a "cut off" failure
    // would land in the same band on every send (PR #291 review 2 L1).
    if stop_reason == "model_context_window_exceeded" {
        return Err(ModelError::Overflow(crate::anthropic::overflow::Overflow {
            window: None,
            input_tokens: None,
            message: "the summary stopped at the model's context window".to_string(),
        }));
    }
    let why = if summary.trim().is_empty() {
        Some("came back empty")
    } else {
        is_cut_off(stop_reason).then_some("was cut off")
    };
    match why {
        Some(why) => Err(ModelError::Other(format!(
            "Couldn't compact the conversation: its summary {why}, so the history was kept as it is. \
             Send another message to try again."
        ))),
        None => Ok(summary),
    }
}

/// What an HTTP kind's honest save appends when the summary stopped
/// mid-sentence (SME-393), whichever stop cut it — the summary's budget
/// (SME-443's `summary_max_tokens`, since SME-443) or the context window
/// (`is_cut_off`) — so the marker's wording names the truncation, not a
/// cause. The save itself is PR #291's decision, kept for the honest
/// cases — failing every summary that stops would fail every send of a
/// long conversation — but a narration that stops mid-sentence must not
/// read as a complete record: 2294639622's seat completed the cut-off
/// sentence with fabrications 16 s after the cut.
pub const SUMMARY_CUT_OFF_MARKER: &str = "\n\n[note: this summary is cut off mid-sentence — \
    what came after the cut was never written. The state it was reaching for is \
    unknown, not settled: verify it with a tool call before acting on it, and always before \
    overwriting, discarding or resetting work.]";

/// One step of an HTTP kind's summarize decision (SME-443): what to do
/// with the summary text given how its call stopped. `summarize` and
/// `continue_summary` answer with these; the whole table is in
/// `http_first` / `http_continued`.
#[derive(Debug, PartialEq, Eq)]
pub enum HttpStep {
    /// Save this text exactly: the summary is whole.
    Save(String),
    /// Save this text with [`SUMMARY_CUT_OFF_MARKER`]: an honest save of
    /// a narration that stopped mid-sentence.
    SaveMarked(String),
    /// The call stopped at the summary's budget with text to continue:
    /// ask once more (SME-443). One `if` at the call site — never a
    /// loop, so a compaction cannot spin (the plan's 2c).
    Continue(String),
}

/// What the one continuation's answer means (SME-443): no `Continue`
/// variant exists — one continuation, ever.
#[derive(Debug, PartialEq, Eq)]
pub enum ContinuedStep {
    /// The joined summary (part 1 + continuation, verbatim) is whole.
    Save(String),
    /// Save the join — or, when the continuation added nothing, part 1
    /// — marked where it stopped.
    SaveMarked(String),
}

/// The HTTP kinds' first summarize call as the SME-443 decision table:
/// text stopped by the window saves marked, unchanged since SME-393 —
/// more budget wouldn't help (SME-73's overflow recovery and SME-393's
/// honesty own this stop); text the summary budget cut off gets the one
/// continuation; anything else is whole. An empty summary still saves
/// (marked when cut) until the refusal step refuses it.
pub fn http_first(summary: String, stop_reason: &str) -> HttpStep {
    match stop_reason {
        // The window, not the budget: no budget continues this.
        "model_context_window_exceeded" => HttpStep::SaveMarked(format!("{summary}{SUMMARY_CUT_OFF_MARKER}")),
        // The budget, with text to continue.
        "max_tokens" if !summary.trim().is_empty() => HttpStep::Continue(summary),
        // Cut by the budget with nothing to continue: today's marked save.
        "max_tokens" => HttpStep::SaveMarked(format!("{summary}{SUMMARY_CUT_OFF_MARKER}")),
        _ => HttpStep::Save(summary),
    }
}

/// The continuation's answer, as the SME-443 table: joined **verbatim**
/// (Q3 decided — no separator; a test pins the join exactly). Stopped by
/// either cut-off stop again, the join saves marked; a continuation that
/// ends its turn with nothing to add leaves part 1 saved marked — it
/// says it is done, and the truncation stands; otherwise the join is
/// whole, and no marker.
pub fn http_continued(part1: &str, continuation: String, stop_reason: &str) -> ContinuedStep {
    match stop_reason {
        "max_tokens" | "model_context_window_exceeded" => {
            ContinuedStep::SaveMarked(format!("{part1}{continuation}{SUMMARY_CUT_OFF_MARKER}"))
        }
        _ if continuation.trim().is_empty() => ContinuedStep::SaveMarked(format!("{part1}{SUMMARY_CUT_OFF_MARKER}")),
        _ => ContinuedStep::Save(format!("{part1}{continuation}")),
    }
}
