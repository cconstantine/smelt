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

mod checkout;
mod queue;
mod start;
mod sweep;

pub(crate) use checkout::*;
pub(crate) use queue::*;
pub(crate) use start::*;
pub(crate) use sweep::*;

/// The longest brief `start_agent` takes, in characters.
pub(crate) const MAX_BRIEF_CHARS: usize = 30_000;

/// The longest report `finish` takes, in characters.
pub(crate) const MAX_REPORT_CHARS: usize = 30_000;

/// The tool a child ends its work with; a successful call ends its turn.
pub(crate) const FINISH: &str = "finish";

/// Why a child can't call `start_agent` (depth 1, until SME-245's limits).
pub(crate) const ROOT_ONLY: &str = "start_agent is only for a conversation without a parent";

/// A persona as `<dir>:<name>`, from its path in the child's checkout
/// (`/workspace/<dir>/.agents/personas/<name>.md`).
pub(crate) fn persona_label(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/workspace/")?;
    let (dir, file) = rest.split_once("/.agents/personas/")?;
    let name = file.strip_suffix(".md")?;
    Some(format!("{dir}:{name}"))
}

/// An agent's status, first that holds: its stored lifecycle when that
/// ends it or hasn't begun, a turn in flight, a question or card waiting
/// on the user, finished, else idle (agents.md, "Statuses", without
/// SME-238's "waiting for its parent").
pub(crate) fn status(state: &str, running: bool, waiting_for_user: bool) -> &'static str {
    match state {
        db::AGENT_STARTING => "starting",
        db::AGENT_FAILED => "failed",
        db::AGENT_STOPPED => "stopped",
        _ if running => "running",
        _ if waiting_for_user => "waiting for the user",
        db::AGENT_FINISHED => "finished",
        _ => "idle",
    }
}

/// `list_agents`' run: the caller's own children, newest first.
pub(crate) async fn list_agents(pool: &PgPool, caller: i64) -> Result<String, String> {
    let children = db::list_child_agents(pool, caller)
        .await
        .map_err(|e| format!("couldn't list your agents: {e}"))?;
    let agents: Vec<serde_json::Value> = children
        .iter()
        .map(|child| {
            serde_json::json!({
                "id": child.id,
                "title": child.title,
                "persona": child.persona_path.as_deref().and_then(persona_label),
                "status": status(&child.state, crate::turn::turn_running(child.id), child.waiting_for_user),
                "created_at": child.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                "last_message_at": child.last_message_at.map(|at| at.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
                "agents_started": child.agents_started,
            })
        })
        .collect();
    Ok(serde_json::json!({"agents": agents}).to_string())
}

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
/// always may; a child still starting, or whose start failed or was
/// stopped, may not. A `finished` or `stopped` child is made `active`
/// again; a `stop_agent` committing after that stops the turn this admits
/// (`stop_conversation` after the update, SME-216's order).
pub(crate) async fn admit_user_turn(pool: &PgPool, conversation_id: i64) -> Result<(), String> {
    let state = db::agent_state(pool, conversation_id)
        .await
        .map_err(|e| format!("couldn't read the agent's state: {e}"))?;
    match state.as_deref() {
        None | Some(db::AGENT_ACTIVE) => Ok(()),
        Some(db::AGENT_STARTING) => Err("This agent is still starting.".to_string()),
        Some(db::AGENT_FAILED) => Err("This agent's start failed; start a new one.".to_string()),
        Some(_) => {
            if db::reactivate_agent(pool, conversation_id)
                .await
                .map_err(|e| format!("couldn't make the agent active again: {e}"))?
            {
                return Ok(());
            }
            // Not finished or stopped after a start any more: read again.
            match db::agent_state(pool, conversation_id)
                .await
                .map_err(|e| format!("couldn't read the agent's state: {e}"))?
                .as_deref()
            {
                Some(db::AGENT_STOPPED) => {
                    Err("This agent was stopped while it was starting; start a new one.".to_string())
                }
                Some(db::AGENT_FAILED) => Err("This agent's start failed; start a new one.".to_string()),
                _ => Ok(()),
            }
        }
    }
}

/// Whether a wake or a delivered notice may run a turn in `conversation_id`
/// now: not while it's a child still starting (its start's own wake comes
/// once it's `active`). A failed read lets it in, so a notice is never
/// dropped.
pub(crate) async fn admits_notices(pool: &PgPool, conversation_id: i64) -> bool {
    match db::agent_state(pool, conversation_id).await {
        Ok(state) => state.as_deref() != Some(db::AGENT_STARTING),
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't read an agent's state before a wake");
            true
        }
    }
}

#[cfg(test)]
pub(crate) mod test_hooks;
#[cfg(test)]
mod tests;
