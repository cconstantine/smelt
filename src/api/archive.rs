//! Archiving a top-level conversation (SME-480), so the sidebar lists only
//! the ones still in use. A view mark only: messages, pods, storage, turns
//! and agents are untouched, and its agents follow it (the tree reports
//! each node's root's mark).

use dioxus::prelude::*;

use crate::models::Conversation;

#[cfg(feature = "server")]
use sqlx::PgPool;

#[post("/api/conversations/{id}/archive")]
pub async fn archive_conversation(id: i64) -> ServerFnResult<Conversation> {
    archive_in(crate::db::get(), id, true).await
}

#[post("/api/conversations/{id}/unarchive")]
pub async fn unarchive_conversation(id: i64) -> ServerFnResult<Conversation> {
    archive_in(crate::db::get(), id, false).await
}

/// Both endpoints' work, taking the pool so a test can use its own: 404
/// for a missing id, 409 naming the root for an agent (D4), and
/// `ConversationsChanged` once the write is done, so every tab's sidebar
/// and header refetch.
#[cfg(feature = "server")]
pub(crate) async fn archive_in(pool: &PgPool, id: i64, archived: bool) -> ServerFnResult<Conversation> {
    match crate::db::set_archived(pool, id, archived).await.map_err(ServerFnError::new)? {
        crate::db::ArchiveOutcome::Done(conversation) => {
            crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
            Ok(conversation)
        }
        crate::db::ArchiveOutcome::NotFound => Err(crate::api::chat::conversation_not_found()),
        crate::db::ArchiveOutcome::NotTopLevel { root_id } => Err(ServerFnError::ServerError {
            message: format!("only a top-level conversation is archived: archive its root, conversation {root_id}"),
            code: 409,
            details: None,
        }),
    }
}

#[cfg(all(test, feature = "server"))]
mod tests;
