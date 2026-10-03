//! The card for the model's `ask_user` (SME-34): what a conversation waits
//! on, answering it, and which conversations wait (the sidebar's mark).

use dioxus::prelude::*;

use crate::questions::{PendingQuestion, QuestionAnswer};

/// The question conversation `id` waits on, if any: the card's snapshot on
/// (re)connect. Live changes arrive as `ConversationEvent::QuestionUpdate`.
#[get("/api/conversations/{id}/question")]
pub async fn get_pending_question(id: i64) -> ServerFnResult<Option<PendingQuestion>> {
    crate::turn::pending_question(crate::db::get(), id).await
}

/// Answers the question `tool_use_id` that conversation `id` waits on, one
/// `QuestionAnswer` per question, and starts the turn that sends it.
#[post("/api/conversations/{id}/question/answer")]
pub async fn answer_question(id: i64, tool_use_id: String, answers: Vec<QuestionAnswer>) -> ServerFnResult<()> {
    crate::turn::answer_question(crate::db::get().clone(), id, tool_use_id, answers).await
}

/// Conversations waiting on an answer, for the sidebar. Refetched on
/// `ConversationEvent::QuestionsChanged`.
#[get("/api/questions/waiting")]
pub async fn get_waiting_conversations() -> ServerFnResult<Vec<i64>> {
    crate::db::list_waiting_conversations(crate::db::get())
        .await
        .map_err(ServerFnError::new)
}
