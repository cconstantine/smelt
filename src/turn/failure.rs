//! How a turn that didn't finish ended (`TurnFailure`), and a turn's
//! result.

use super::*;

/// How a turn that didn't finish ended: the user stopped it, or it
/// failed.
#[derive(Debug)]
pub(crate) enum TurnFailure {
    Stopped,
    Failed(ServerFnError),
}

impl TurnFailure {
    /// The text a tab is shown (`TurnError`): `TURN_STOPPED` for a stop,
    /// which the page shows as "Stopped.".
    pub(crate) fn message(&self) -> String {
        match self {
            TurnFailure::Stopped => TURN_STOPPED.to_string(),
            TurnFailure::Failed(e) => chat_error_text(e),
        }
    }
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnFailure::Stopped => f.write_str(TURN_STOPPED),
            TurnFailure::Failed(e) => e.fmt(f),
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
