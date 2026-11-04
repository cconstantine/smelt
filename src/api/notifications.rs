//! SME-71: the one endpoint a browser asks before it notifies — "is this
//! conversation still waiting on the user, and what would the notification
//! say?" The event stream only announces that something ended; this is the
//! check that it is still true (and what the card would name) at the
//! moment a notification would appear.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::events::Attention;

/// What a notification says: the heading comes from `attention`, the body
/// is the title, plus the first question's header for a question, or the
/// model's `notify_user` message for a `Notified` (SME-377).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AttentionDetails {
    pub title: String,
    pub attention: Attention,
    pub question_header: Option<String>,
    /// Defaulted, so a tab reads an older server's answer.
    #[serde(default)]
    pub message: Option<String>,
}

/// The attention event `attention` (of the turn whose generation is
/// `generation`) announced is still worth a notification. Query arguments
/// as `api/providers.rs`'s `?ask_each`; the leader passes the event's kind
/// back because the server stops holding it once published.
#[get("/api/conversations/{id}/attention?generation&attention")]
pub async fn get_attention(id: i64, generation: u64, attention: Attention) -> ServerFnResult<Option<AttentionDetails>> {
    attention_details(crate::db::get(), id, generation, attention).await
}

/// `get_attention`'s decision on `pool` — `None` when, between the event
/// and this call, it stopped being true:
/// - a turn is running again (`turn_running`);
/// - the generation has moved on (`turn_generation`): the user wrote,
///   answered, or another turn took one;
/// - for a question, nothing waits any more: answered, dismissed, or a row
///   asked of the child's parent, which no card can answer (Revision 6);
/// - for a `Notified` (SME-377), the runtime no longer holds the model's
///   message for exactly that generation: a restart, or a newer notice;
/// - the conversation is gone, so it has no title.
#[cfg(feature = "server")]
pub(crate) async fn attention_details(pool: &sqlx::PgPool, id: i64, generation: u64, attention: Attention) -> ServerFnResult<Option<AttentionDetails>> {
    use crate::db;
    if crate::turn::turn_running(id) || crate::turn::turn_generation(id) != generation {
        return Ok(None);
    }
    // `pending_question` is the one question check: it answers nothing for
    // a row already answered or asked of the parent.
    let question = if attention == Attention::Question {
        let question = crate::turn::pending_question(pool, id).await?;
        if question.is_none() {
            return Ok(None);
        }
        question
    } else {
        None
    };
    let message = if attention == Attention::Notified {
        let message = crate::turn::published_notice(id, generation);
        if message.is_none() {
            return Ok(None);
        }
        message
    } else {
        None
    };
    let title = db::get_conversation_title(pool, id).await.map_err(ServerFnError::new)?;
    Ok(title.map(|title| AttentionDetails {
        title,
        attention,
        question_header: question.and_then(|q| q.questions.into_iter().next().map(|question| question.header)),
        message,
    }))
}

#[cfg(test)]
mod tests;
