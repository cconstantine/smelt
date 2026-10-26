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
mod delete;
mod messages;
mod persona;
mod queue;
mod start;
mod sweep;

pub(crate) use checkout::*;
pub(crate) use delete::*;
pub(crate) use messages::*;
pub(crate) use queue::*;
pub(crate) use start::*;
pub(crate) use sweep::*;

/// The longest brief `start_agent` takes, in characters.
pub(crate) const MAX_BRIEF_CHARS: usize = 30_000;

/// The longest report `finish` takes, in characters.
pub(crate) const MAX_REPORT_CHARS: usize = 30_000;

/// The longest message `message_agent` takes, in characters.
pub(crate) const MAX_MESSAGE_CHARS: usize = 30_000;

/// The tool a child asks its parent with (SME-238); a successful call
/// ends its turn, as `ask_user`'s does.
pub(crate) const ASK_PARENT: &str = "ask_parent";

/// The tool a child ends its work with; a successful call ends its turn.
pub(crate) const FINISH: &str = "finish";

/// `SMELT_AGENT_DEPTH`'s default (SME-241): how many levels of agents
/// below the user's own conversation `start_agent` may go. An env-set
/// ceiling, like `SANDBOX_IDLE_NOTICE_MINUTES`: read at each call, and
/// an unparseable setting falls back to this with a warning. (SME-245
/// adds the persona's own limit on top; until then this is the only one.)
pub(crate) const DEFAULT_AGENT_DEPTH: i32 = 5;

/// The depth ceiling for a `start_agent` call now.
pub(crate) fn agent_depth() -> i32 {
    agent_depth_from(std::env::var("SMELT_AGENT_DEPTH").ok())
}

/// The ceiling for a `SMELT_AGENT_DEPTH` of `setting`. Pulled out of
/// `agent_depth` so the parsing is testable without the environment; an
/// empty setting is no setting, and anything that isn't a level count of
/// 1 or more gets a warning — the operator set it, and silently falling
/// back would hide that it isn't being used.
pub(crate) fn agent_depth_from(setting: Option<String>) -> i32 {
    let Some(setting) = setting.filter(|s| !s.is_empty()) else {
        return DEFAULT_AGENT_DEPTH;
    };
    match setting.parse::<i32>() {
        Ok(levels) if levels >= 1 => levels,
        _ => {
            tracing::warn!(
                setting = %setting,
                default = DEFAULT_AGENT_DEPTH,
                "SMELT_AGENT_DEPTH is not a level count of 1 or more; using the default"
            );
            DEFAULT_AGENT_DEPTH
        }
    }
}

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
/// on the user, a question to its parent (SME-238), finished, else idle
/// (agents.md, "Statuses").
pub(crate) fn status(state: &str, running: bool, waiting_for_user: bool, waiting_for_parent: bool) -> &'static str {
    match state {
        db::AGENT_STARTING => "starting",
        db::AGENT_FAILED => "failed",
        db::AGENT_STOPPED => "stopped",
        _ if running => "running",
        _ if waiting_for_user => "waiting for the user",
        _ if waiting_for_parent => "waiting for its parent",
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
                "status": status(
                    &child.state,
                    crate::turn::turn_running(child.id),
                    child.waiting_for_user,
                    child.waiting_for_parent
                ),
                "created_at": child.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                "last_message_at": child.last_message_at.map(|at| at.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
                "agents_started": child.agents_started,
            })
        })
        .collect();
    Ok(serde_json::json!({"agents": agents}).to_string())
}

/// How agent `id` is named to its parent, with its persona when it has one.
pub(crate) async fn label_of(pool: &PgPool, id: i64) -> String {
    let persona = match db::agent_persona(pool, id).await {
        Ok(persona) => persona.and_then(|p| persona_label(&p.persona_path)),
        Err(e) => {
            tracing::warn!(conversation_id = id, error = %e, "couldn't read an agent's persona for its label");
            None
        }
    };
    label(id, persona.as_deref())
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
/// again. The caller then counts the turn and calls `confirm_user_turn`,
/// so a `stop_agent` landing between this and the count is seen.
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

/// The user's turn, once it holds the turn lock: an agent that finished
/// while the turn waited behind its last one is `active` again, so the
/// work the user asked for can be reported too (review 2).
pub(crate) async fn resume_finished(pool: &PgPool, conversation_id: i64) {
    if let Err(e) = db::reactivate_finished(pool, conversation_id).await {
        tracing::warn!(conversation_id, error = %e, "couldn't make a finished agent active for the user's turn");
    }
}

/// After `admit_user_turn` and the turn's count: refused if a `stop_agent`
/// (or a delete) landed in between, whose `stop_conversation` then found no
/// turn to end (review 1). One landing after this read stops the counted
/// turn itself (SME-216's order). A failed read lets the turn go on.
pub(crate) async fn confirm_user_turn(pool: &PgPool, conversation_id: i64) -> Result<(), String> {
    match db::agent_state(pool, conversation_id).await {
        Ok(Some(state)) if state == db::AGENT_STOPPED => Err("This agent was stopped by its parent.".to_string()),
        Ok(Some(state)) if state == db::AGENT_FAILED || state == db::AGENT_STARTING => {
            Err(format!("This agent can't take a message now (it's {state})."))
        }
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't read an agent's state after admitting the user's turn");
            Ok(())
        }
    }
}

/// Whether a wake or a delivered notice may run a turn in `conversation_id`
/// now: not while it's a child still starting (its start's own wake comes
/// once it's `active`), nor one stopped or failed, whose pause a restart
/// may have lost (review 1): a notice to it is only saved, and only the
/// user writing in a stopped one brings it back (`admit_user_turn`). A
/// failed read lets it in, so a notice is never dropped.
pub(crate) async fn admits_notices(pool: &PgPool, conversation_id: i64) -> bool {
    match db::agent_state(pool, conversation_id).await {
        Ok(state) => !matches!(state.as_deref(), Some(db::AGENT_STARTING | db::AGENT_STOPPED | db::AGENT_FAILED)),
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
