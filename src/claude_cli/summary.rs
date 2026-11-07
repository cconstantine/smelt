//! What a compaction does with the summary a Claude Code call returned
//! (SME-431, PR #291's reviews). The CLI's summary shares its cap with
//! thinking that can't be turned off, so it can come back empty or cut
//! off; saved, it would replace the history with nothing or a stub.

use crate::anthropic::stream::{is_cut_off, ModelError};
use crate::providers::ProviderKind;

/// The summary, or why it isn't saved over the history. Only the Claude
/// Code kind is checked (the user's Q1): an HTTP kind's summary has no
/// thinking in its cap, and one cut off at that cap is saved as it always
/// was, since failing it would fail every send of a long conversation.
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
