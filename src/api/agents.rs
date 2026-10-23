//! The sidebar's conversation tree and the "agents waiting" banner over a
//! parent's chat (SME-246): every conversation with what its row shows of
//! an agent, and which agents under a conversation wait for the user.

use chrono::NaiveDateTime;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// One conversation as the sidebar shows it. The client lays the list out
/// as a tree by `parent_id` (`frontend::pages::chat::sidebar::tree`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversationNode {
    pub id: i64,
    pub title: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    /// The conversation that started it as its agent; `None` for the
    /// user's own.
    pub parent_id: Option<i64>,
    /// An agent's persona, `<dir>:<name>` (the name alone while it starts);
    /// `None` without one.
    pub persona: Option<String>,
    /// An agent's status, as `list_agents` gives it (agents.md,
    /// "Statuses"); `None` for the user's own conversations.
    pub status: Option<String>,
    /// A turn is running or queued (the "Working" mark).
    pub busy: bool,
    /// An `ask_user` question or an AGENTS.md trust card waits for the
    /// user (the "?" mark).
    pub waiting_for_user: bool,
}

/// One agent under a conversation that waits for the user, for its banner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentWaiting {
    pub id: i64,
    pub title: String,
}

/// A conversation just made, before the next fetch brings its node: the
/// user's own, so no persona or status, and nothing running or waiting.
impl From<crate::models::Conversation> for ConversationNode {
    fn from(conversation: crate::models::Conversation) -> Self {
        ConversationNode {
            id: conversation.id,
            title: conversation.title,
            created_at: conversation.created_at,
            updated_at: conversation.updated_at,
            parent_id: conversation.parent_id,
            persona: None,
            status: None,
            busy: false,
            waiting_for_user: false,
        }
    }
}

/// Every conversation, most recently active first, with its agent's
/// persona and status. Refetched on `ConversationsChanged`,
/// `TurnsChanged` and `QuestionsChanged`: the status is derived here at
/// each call, never stored, so any of them brings every row up to date.
#[get("/api/conversations/tree")]
pub async fn get_conversation_tree() -> ServerFnResult<Vec<ConversationNode>> {
    let rows = crate::db::conversation_tree(crate::db::get())
        .await
        .map_err(ServerFnError::new)?;
    let running: std::collections::HashSet<i64> = crate::turn::busy_conversations().into_iter().collect();
    Ok(rows.into_iter().map(|row| node(row, &running)).collect())
}

/// The agents under conversation `id`, at any depth, waiting for the user
/// (a question or a trust card; never a question to their parent).
#[get("/api/conversations/{id}/agents-waiting")]
pub async fn get_agents_waiting(id: i64) -> ServerFnResult<Vec<AgentWaiting>> {
    let waiting = crate::db::agents_waiting_under(crate::db::get(), id)
        .await
        .map_err(ServerFnError::new)?;
    Ok(waiting.into_iter().map(|(id, title)| AgentWaiting { id, title }).collect())
}

/// A tree row as the sidebar shows it, `running` being the conversations
/// with a turn in flight.
#[cfg(feature = "server")]
fn node(row: crate::db::TreeRow, running: &std::collections::HashSet<i64>) -> ConversationNode {
    let busy = running.contains(&row.id);
    let persona = row
        .persona_path
        .as_deref()
        .and_then(crate::agents::persona_label)
        .or(row.persona_name);
    let status = row
        .state
        .as_deref()
        .map(|state| crate::agents::status(state, busy, row.waiting_for_user, row.waiting_for_parent).to_string());
    ConversationNode {
        id: row.id,
        title: row.title,
        created_at: row.created_at,
        updated_at: row.updated_at,
        parent_id: row.parent_id,
        persona,
        status,
        busy,
        waiting_for_user: row.waiting_for_user,
    }
}

#[cfg(all(test, feature = "server"))]
mod tests;
