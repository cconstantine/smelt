//! What a compaction does with the summary its model call returned
//! (SME-431, PR #291's reviews; the HTTP kinds' decisions since
//! SME-443). The CLI's summary shares its cap with thinking that can't
//! be turned off, so it can come back empty or cut off; saved, it would
//! replace the history with nothing or a stub. An HTTP kind's summary
//! cut off at its (room-derived, SME-443) budget is continued once
//! instead of saved mid-sentence; what the exchange yields saves whole,
//! or saves marked where it still stopped — SME-393's honesty. What
//! still cannot make a whole record is refused — an empty summary on
//! every kind, and one cut off again after the single continuation —
//! the history kept as it is and the turn ending with the notice
//! (SME-431's shape).

use crate::anthropic::stream::{is_cut_off, ModelError};
use crate::providers::ProviderKind;

/// The summary, or why it isn't saved over the history. An empty
/// summary is refused on every kind — HTTP kinds since SME-443, on any
/// stop (a reasoning-only body is empty too); it would replace the
/// history with nothing. Otherwise Claude Code refuses its cut-offs
/// (the user's Q1, SME-431), and an HTTP kind's cut-off summary is
/// continued once rather than saved mid-sentence — `http_first` and
/// `http_continued` below are that decision, and what they hand back
/// saves as it always did.
pub fn checked(kind: ProviderKind, summary: String, stop_reason: &str) -> Result<String, ModelError> {
    if kind != ProviderKind::ClaudeCli {
        // HTTP kinds (SME-443 step 3): empty is refused before any
        // continuation — there is nothing to continue (A6, A7).
        if summary.trim().is_empty() {
            return Err(refused("came back empty"));
        }
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
        Some(why) => Err(refused(why)),
        None => Ok(summary),
    }
}

/// A refused summary, in the shape and words SME-431 shipped (its tests
/// pin them) — the typed message the failed turn ends with, history
/// unchanged (no divider written; `compact` never reaches its save).
fn refused(why: &str) -> ModelError {
    ModelError::Other(format!(
        "Couldn't compact the conversation: its summary {why}, so the history was kept as it is. \
         Send another message to try again."
    ))
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
    /// Cut off **again** after the one continuation: refuse the whole
    /// compaction with this typed message (SME-443 step 3) — two calls
    /// is the bound, and a marked torso of a record is what the seat
    /// must not get (SME-393's lesson, now at the second cut).
    Fail(String),
}

/// The HTTP kinds' first summarize call as the SME-443 decision table.
/// An empty summary never reaches here — `checked` refuses it (SME-443
/// step 3). Text stopped by the window saves marked, unchanged since
/// SME-393 — more budget wouldn't help (SME-73's overflow recovery and
/// SME-393's honesty own this stop); text the summary budget cut off
/// gets the one continuation; anything else is whole.
pub fn http_first(summary: String, stop_reason: &str) -> HttpStep {
    match stop_reason {
        // The window, not the budget: no budget continues this.
        "model_context_window_exceeded" => HttpStep::SaveMarked(format!("{summary}{SUMMARY_CUT_OFF_MARKER}")),
        // The budget, with text — checked refused the empty — to continue.
        "max_tokens" => HttpStep::Continue(summary),
        _ => HttpStep::Save(summary),
    }
}

/// The continuation's answer, as the SME-443 table: joined **verbatim**
/// (Q3 decided — no separator; a test pins the join exactly). Cut off at
/// the budget **again**, the compaction is refused — `Fail`, the plan's
/// bound (at most 2 calls, then a loud notice; no marked torso).
/// Stopped by the window, the join saves marked (SME-393's honest row);
/// a continuation that ends its turn with nothing to add leaves part 1
/// saved marked — it says it is done, and the truncation stands.
/// Otherwise the join is whole, and no marker.
pub fn http_continued(part1: &str, continuation: String, stop_reason: &str) -> ContinuedStep {
    match stop_reason {
        "max_tokens" => ContinuedStep::Fail(
            "Couldn't compact the conversation: its summary was cut off again after one continuation, \
             so the history was kept as it is. Send another message to try again; if it keeps \
             happening, give the model a larger window or pick one that writes whole summaries."
                .to_string(),
        ),
        "model_context_window_exceeded" => {
            ContinuedStep::SaveMarked(format!("{part1}{continuation}{SUMMARY_CUT_OFF_MARKER}"))
        }
        _ if continuation.trim().is_empty() => ContinuedStep::SaveMarked(format!("{part1}{SUMMARY_CUT_OFF_MARKER}")),
        _ => ContinuedStep::Save(format!("{part1}{continuation}")),
    }
}
