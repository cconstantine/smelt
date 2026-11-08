//! A conversation owed an answer (SME-450): its turn saved something for
//! the model — a drained notice, an answer taken for a turn that isn't
//! the user's own — that the model hasn't answered yet.
//!
//! `conversations.answer_owed_at` is set in the transaction that saves
//! the message it's for, and cleared in the reply's transaction and at the
//! turn's end, all under the turn lock. So in a running server it's set
//! only while the turn that set it runs; set with no turn running, a
//! restart (or a panic) dropped that turn, and the startup sweep and the
//! next wake answer it, once (`wake_conversation`, `run_turn_steps`).

use sqlx::PgPool;

use super::conversations::create_message_on;
use crate::anthropic::ContentBlock;
use crate::models::Message;

/// The conversations a startup wake may run in: not paused by the user,
/// not being deleted, and not an agent that's stopped, failed or still
/// starting (the sweep fails those first; `admits_notices` re-checks).
/// `conversations` must be aliased `c`, joined to `agents a` by a
/// `LEFT JOIN`.
const WAKEABLE: &str = "c.paused_at IS NULL AND c.deleting_at IS NULL \
    AND (a.state IS NULL OR a.state NOT IN ('stopped', 'failed', 'starting'))";

/// Marks `conversation_id` owed an answer, keeping an earlier mark.
pub(super) async fn mark_answer_owed_on(conn: &mut sqlx::PgConnection, conversation_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversations SET answer_owed_at = COALESCE(answer_owed_at, now()) WHERE id = $1")
        .bind(conversation_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Saves `content` as a `user` message the model owes an answer, and the
/// mark, on `conn` (the caller's transaction).
pub(super) async fn create_owed_message_on(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    content: &[ContentBlock],
) -> Result<Message, sqlx::Error> {
    let saved = create_message_on(&mut *conn, conversation_id, "user", content).await?;
    mark_answer_owed_on(conn, conversation_id).await?;
    Ok(saved)
}

/// Saves the model's reply and clears the mark, in one transaction: once
/// the reply is saved nothing before it is owed, whether the turn ends
/// or a restart lands first.
pub async fn save_reply_message(pool: &PgPool, conversation_id: i64, content: &[ContentBlock]) -> Result<Message, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let saved = create_message_on(&mut tx, conversation_id, "assistant", content).await?;
    clear_answer_owed_on(&mut tx, conversation_id).await?;
    tx.commit().await?;
    Ok(saved)
}

async fn clear_answer_owed_on(conn: &mut sqlx::PgConnection, conversation_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversations SET answer_owed_at = NULL WHERE id = $1 AND answer_owed_at IS NOT NULL")
        .bind(conversation_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Clears the mark: the turn that set it has ended, however it ended (D7).
pub async fn clear_answer_owed(pool: &PgPool, conversation_id: i64) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    clear_answer_owed_on(&mut conn, conversation_id).await
}

/// Whether `conversation_id` is owed an answer.
pub async fn answer_owed(pool: &PgPool, conversation_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM conversations WHERE id = $1 AND answer_owed_at IS NOT NULL)")
        .bind(conversation_id)
        .fetch_one(pool)
        .await
}

/// Every conversation owed an answer that a wake may run in, roots and
/// agents alike: the startup sweep wakes them.
pub async fn conversations_owed_an_answer(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT c.id FROM conversations c LEFT JOIN agents a ON a.conversation_id = c.id
         WHERE c.answer_owed_at IS NOT NULL AND {WAKEABLE}
         ORDER BY c.id"
    ))
    .fetch_all(pool)
    .await
}

/// Every conversation a wake may run in with a command whose end is
/// recorded and whose notice no drain has saved (`UNNOTIFIED_END`): its
/// wake died before draining, in a restart (SME-450 E). The startup sweep
/// wakes them, and their drain saves the notice as any wake's would.
pub async fn conversations_with_unnotified_ends(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT c.id FROM conversations c LEFT JOIN agents a ON a.conversation_id = c.id
         WHERE {WAKEABLE}
           AND EXISTS (SELECT 1 FROM terminal_commands tc WHERE tc.conversation_id = c.id AND {})
         ORDER BY c.id",
        super::sandbox::UNNOTIFIED_END
    ))
    .fetch_all(pool)
    .await
}
