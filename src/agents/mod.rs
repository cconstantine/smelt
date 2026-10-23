//! Child agents (SME-236): a conversation's model starts a child
//! conversation that works on a brief in the background and reports back.
//! A child is a whole conversation with a `parent_id`, an `agents` row
//! holding its lifecycle (`starting`, `active`, `finished`, `stopped`,
//! `failed`), and its own sandbox. Reports and notices reach a
//! conversation through `agent_messages`, drained under the recipient's
//! turn lock (`turn::drain_agent_messages`) and woken with `Notice::Wake`.
//!
//! The design is `docs/design/personas/agents.md`; the state model and
//! lock order are in this ticket's plan.

use sqlx::PgPool;

use crate::db;

mod queue;
mod start;

pub(crate) use queue::*;
pub(crate) use start::*;

/// The longest brief `start_agent` takes, in characters.
pub(crate) const MAX_BRIEF_CHARS: usize = 30_000;

/// The longest report `finish` takes, in characters.
pub(crate) const MAX_REPORT_CHARS: usize = 30_000;

/// The tool a child ends its work with; a successful call ends its turn.
pub(crate) const FINISH: &str = "finish";

/// Why a child can't call `start_agent` (depth 1, until SME-245's limits).
pub(crate) const ROOT_ONLY: &str = "start_agent is only for a conversation without a parent";

/// How an agent is named to its parent: `agent #12`, or with its persona,
/// `agent #12 (smelt:developer)`.
pub(crate) fn label(conversation_id: i64, persona: Option<&str>) -> String {
    match persona {
        Some(persona) => format!("agent #{conversation_id} ({persona})"),
        None => format!("agent #{conversation_id}"),
    }
}

/// Whether the user's send or answer may start a turn in `conversation_id`,
/// checked before the turn is counted: a conversation that isn't an agent
/// always may; a child still starting may not.
pub(crate) async fn admit_user_turn(pool: &PgPool, conversation_id: i64) -> Result<(), String> {
    let state = db::agent_state(pool, conversation_id)
        .await
        .map_err(|e| format!("couldn't read the agent's state: {e}"))?;
    match state.as_deref() {
        Some(db::AGENT_STARTING) => Err("This agent is still starting.".to_string()),
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod test_hooks;
#[cfg(test)]
mod tests;
