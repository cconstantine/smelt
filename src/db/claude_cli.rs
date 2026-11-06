use super::*;

// --- Claude Code sessions (SME-430) ---

/// A Claude Code conversation's CLI session: the one its next turn
/// resumes, on provider `provider_id`, which has had the request's first
/// `synced_count` messages (hashed as `synced_hash`) and the reply to them.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ClaudeCliSession {
    pub provider_id: i64,
    pub session_id: String,
    pub synced_count: i32,
    pub synced_hash: String,
}

/// `conversation_id`'s session, if it has one.
pub async fn get_claude_cli_session(pool: &PgPool, conversation_id: i64) -> Result<Option<ClaudeCliSession>, sqlx::Error> {
    sqlx::query_as("SELECT provider_id, session_id, synced_count, synced_hash FROM claude_cli_sessions WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_optional(pool)
        .await
}

/// Records `session` as `conversation_id`'s, replacing any it had.
pub async fn set_claude_cli_session(pool: &PgPool, conversation_id: i64, session: &ClaudeCliSession) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO claude_cli_sessions (conversation_id, provider_id, session_id, synced_count, synced_hash, updated_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (conversation_id) DO UPDATE SET
             provider_id = EXCLUDED.provider_id,
             session_id = EXCLUDED.session_id,
             synced_count = EXCLUDED.synced_count,
             synced_hash = EXCLUDED.synced_hash,
             updated_at = now()",
    )
    .bind(conversation_id)
    .bind(session.provider_id)
    .bind(&session.session_id)
    .bind(session.synced_count)
    .bind(&session.synced_hash)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Forgets `conversation_id`'s session: its next turn starts a new one.
pub async fn delete_claude_cli_session(pool: &PgPool, conversation_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM claude_cli_sessions WHERE conversation_id = $1")
        .bind(conversation_id)
        .execute(pool)
        .await
        .map(|_| ())
}
