//! Compacting a conversation on request (SME-482): the chat's Compact
//! button and any script call this one route. Its own file, out of
//! `chat.rs`'s way.

use dioxus::prelude::*;

/// Compacts the conversation now, between turns, with an optional `note`
/// of what the summary must keep. Refused while a turn runs or is queued,
/// while a question waits, when the last reply's calls have no results,
/// and when nothing is new since the last compaction
/// (`turn::compact_now`).
#[post("/api/conversations/{id}/compact")]
pub async fn compact_conversation_now(id: i64, note: Option<String>) -> ServerFnResult<()> {
    crate::turn::compact_now(crate::db::get(), id, note).await
}
