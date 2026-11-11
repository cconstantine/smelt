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
    /// When it last ended its turn owing its parent a report with nothing
    /// running, `Some` only while that is still true — the tree gates
    /// `agents.report_owed_at` on the stuck predicate, so the mark
    /// self-hides while the seat runs something and returns with its
    /// original age (SME-455).
    pub report_owed_at: Option<NaiveDateTime>,
}

/// Every conversation, most recently active first.
pub async fn conversation_tree(pool: &PgPool) -> Result<Vec<TreeRow>, sqlx::Error> {
    sqlx::query_as::<_, TreeRow>(&format!(
        "SELECT c.id, c.title, c.created_at, c.updated_at, c.parent_id,
                a.state, a.persona_name, a.persona_path,
                {} AS waiting_for_user,
                EXISTS (SELECT 1 FROM pending_questions q
                        WHERE q.conversation_id = c.id AND q.answer IS NULL AND q.asked_of = $1) AS waiting_for_parent,
                {} AS report_owed_at
         FROM conversations c LEFT JOIN agents a ON a.conversation_id = c.id
         ORDER BY c.updated_at DESC, c.id DESC",
        super::WAITS_FOR_USER,
        super::REPORT_OWED_CASE
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

/// Whether an agent under `id`, at any depth, is `starting` or `active` on
/// provider `provider_id` (its own, or the default it'll take): the agents
/// a turn of `id`'s gives its request slot up to across a tool pass, so a
/// one-slot provider doesn't keep them waiting for that whole turn (SME-354
/// B6).
pub async fn live_agent_under_on_provider(pool: &PgPool, id: i64, provider_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "WITH RECURSIVE under(id) AS (
             SELECT id FROM conversations WHERE parent_id = $1
             UNION
             SELECT g.id FROM conversations g JOIN under u ON g.parent_id = u.id
         )
         SELECT EXISTS (
             SELECT 1 FROM under u
             JOIN conversations c ON c.id = u.id
             JOIN agents a ON a.conversation_id = u.id
             WHERE a.state IN ($2, $3)
               AND COALESCE(c.provider_id, (SELECT default_provider_id FROM inference_settings)) = $4
         )",
    )
    .bind(id)
    .bind(AGENT_STARTING)
    .bind(AGENT_ACTIVE)
    .bind(provider_id)
    .fetch_one(pool)
    .await
}
