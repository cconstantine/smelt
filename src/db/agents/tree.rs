//! The conversation tree the sidebar shows (SME-246): every conversation
//! with what its row needs for an agent's persona and status, and the
//! agents under a conversation that wait for the user.

use super::*;

/// One conversation as the sidebar's tree reads it.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct TreeRow {
    pub id: i64,
    pub title: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub parent_id: Option<i64>,
    /// Its `agents.state`; `None` for the user's own conversations.
    pub state: Option<String>,
    pub persona_name: Option<String>,
    pub persona_path: Option<String>,
    /// A question to the user is unanswered, or an AGENTS.md trust card waits.
    pub waiting_for_user: bool,
    /// Its `ask_parent` question is unanswered (SME-238).
    pub waiting_for_parent: bool,
}

/// Every conversation, most recently active first.
pub async fn conversation_tree(pool: &PgPool) -> Result<Vec<TreeRow>, sqlx::Error> {
    sqlx::query_as::<_, TreeRow>(&format!(
        "SELECT c.id, c.title, c.created_at, c.updated_at, c.parent_id,
                a.state, a.persona_name, a.persona_path,
                {} AS waiting_for_user,
                EXISTS (SELECT 1 FROM pending_questions q
                        WHERE q.conversation_id = c.id AND q.answer IS NULL AND q.asked_of = $1) AS waiting_for_parent
         FROM conversations c LEFT JOIN agents a ON a.conversation_id = c.id
         ORDER BY c.updated_at DESC, c.id DESC",
        super::WAITS_FOR_USER
    ))
    .bind(super::ASKED_OF_PARENT)
    .fetch_all(pool)
    .await
}

/// The conversations under `id`, at any depth, that wait for the user,
/// by id: `(id, title)`.
pub async fn agents_waiting_under(pool: &PgPool, id: i64) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String)>(&format!(
        "WITH RECURSIVE under(id) AS (
             SELECT id FROM conversations WHERE parent_id = $1
             UNION
             SELECT g.id FROM conversations g JOIN under u ON g.parent_id = u.id
         )
         SELECT c.id, c.title FROM conversations c JOIN under u ON u.id = c.id
         WHERE {}
         ORDER BY c.id",
        super::WAITS_FOR_USER
    ))
    .bind(id)
    .fetch_all(pool)
    .await
}
