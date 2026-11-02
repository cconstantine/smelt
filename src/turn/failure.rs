//! How a turn that didn't finish ended (`TurnFailure`), and a turn's
//! result.

use super::*;

/// How a turn that didn't finish ended: the user stopped it, or it
/// failed.
#[derive(Debug)]
pub(crate) enum TurnFailure {
    Stopped,
    Failed(ServerFnError),
    /// A user's turn that failed with `reason`, its failure notice saved in
    /// the conversation while the turn still held the lock (SME-123 DR3,
    /// review 1 M1: saved after it, a turn queued behind could save its own
    /// message first, and the notice landed under the newer message).
    Noted { reason: String },
}

impl TurnFailure {
    /// The text a tab is shown (`TurnError`): `TURN_STOPPED` for a stop,
    /// which the page shows as "Stopped.".
    pub(crate) fn message(&self) -> String {
        match self {
            TurnFailure::Stopped => TURN_STOPPED.to_string(),
            TurnFailure::Failed(e) => chat_error_text(e),
            TurnFailure::Noted { .. } => TURN_FAILED_NOTED.to_string(),
        }
    }
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnFailure::Stopped => f.write_str(TURN_STOPPED),
            TurnFailure::Failed(e) => e.fmt(f),
            TurnFailure::Noted { reason } => f.write_str(reason),
        }
    }
}

impl From<ServerFnError> for TurnFailure {
    fn from(e: ServerFnError) -> Self {
        TurnFailure::Failed(e)
    }
}

/// A turn's messages, saved in order, or how it ended early.
pub(crate) type TurnResult = Result<Vec<Message>, TurnFailure>;
