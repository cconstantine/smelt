use super::*;

pub(super) const DEFAULT_TITLE: &str = "New conversation";

pub async fn create_conversation(pool: &PgPool) -> Result<Conversation, sqlx::Error> {
    sqlx::query_as::<_, Conversation>("INSERT INTO conversations (title) VALUES ($1) RETURNING *")
        .bind(DEFAULT_TITLE)
        .fetch_one(pool)
        .await
}

pub async fn list_conversations(pool: &PgPool) -> Result<Vec<Conversation>, sqlx::Error> {
    sqlx::query_as::<_, Conversation>("SELECT * FROM conversations ORDER BY updated_at DESC")
        .fetch_all(pool)
        .await
}

pub async fn list_messages(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<Message>, sqlx::Error> {
    sqlx::query_as::<_, Message>(
        "SELECT * FROM messages WHERE conversation_id = $1 ORDER BY created_at ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// Plain-text stand-in for the conversation title, extracted from a message's
/// `Text` blocks only (`ToolUse`/`ToolResult` blocks carry nothing sensible
/// to show as a title). Concatenates every `Text` block since a message is
/// modeled as content *blocks*, not necessarily a single one.
fn title_candidate(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn create_message(
    pool: &PgPool,
    conversation_id: i64,
    role: &str,
    content: &[ContentBlock],
) -> Result<Message, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    create_message_on(&mut conn, conversation_id, role, content).await
}

/// `create_message` on a given connection, so a caller can make it part
/// of a transaction of its own (`save_command_notice`).
pub(super) async fn create_message_on(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    role: &str,
    content: &[ContentBlock],
) -> Result<Message, sqlx::Error> {
    #[expect(clippy::expect_used, reason = "a ContentBlock always serializes: its map keys are strings, and a serde_json::Value holds only finite numbers")]
    let content_json = serde_json::to_string(content)
        .expect("ContentBlock always serializes: no non-string map keys, no floats");

    let message = sqlx::query_as::<_, Message>(
        "INSERT INTO messages (conversation_id, role, content) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(conversation_id)
    .bind(role)
    .bind(&content_json)
    .fetch_one(&mut *conn)
    .await?;

    // Bump updated_at so the sidebar can sort by recency; auto-title the
    // conversation from the first user message if it's still the default (in
// either spelling: before SME-41 the default was "New Conversation").
    sqlx::query(
        "UPDATE conversations
         SET updated_at = now(),
             title = CASE
                 WHEN lower(title) = lower($1) AND $2 = 'user' THEN left($3, 60)
                 ELSE title
             END
         WHERE id = $4",
    )
    .bind(DEFAULT_TITLE)
    .bind(role)
    .bind(title_candidate(content))
    .bind(conversation_id)
    .execute(&mut *conn)
    .await?;

    Ok(message)
}

pub async fn conversation_exists(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM conversations WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// Mirrors the in-memory pause after a Stop into `paused_at` (SME-238):
/// set (keeping when it was first set) or cleared.
pub async fn set_paused(pool: &PgPool, id: i64, paused: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversations SET paused_at = CASE WHEN $2 THEN COALESCE(paused_at, now()) END WHERE id = $1")
        .bind(id)
        .bind(paused)
        .execute(pool)
        .await?;
    Ok(())
}

/// Every conversation whose pause is stored, for the start's restore.
pub async fn paused_conversations(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM conversations WHERE paused_at IS NOT NULL ORDER BY id")
        .fetch_all(pool)
        .await
}

/// A plain delete, for tests: the app deletes through `agents::delete_subtree`.
#[cfg(test)]
pub async fn delete_conversation(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Last-known real `usage` numbers for `conversation_id` — see
/// SME-18. `None` if no turn has completed
/// for this conversation yet (nothing to report).
pub async fn get_conversation_usage(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<crate::anthropic::TokenUsage>, sqlx::Error> {
    sqlx::query_as::<_, crate::anthropic::TokenUsage>(
        "SELECT input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens
         FROM conversation_context_usage WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
}

/// Forgets `conversation_id`'s last known usage, after a compaction made
/// it describe a history that's no longer sent (SME-51 B10). Holding the
/// conversation's row first, as `record_model_call_if_current` does, so
/// an older call's record still on its way commits before the clear
/// rather than after it (SME-178 code review 1).
pub async fn clear_conversation_usage(pool: &PgPool, conversation_id: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM conversations WHERE id = $1 FOR NO KEY UPDATE")
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM conversation_context_usage WHERE conversation_id = $1")
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Overwrites `conversation_id`'s usage row with `usage` — "last known
/// only," not a history, so this is always a full replace, not an
/// accumulation.
pub async fn upsert_conversation_usage(
    executor: impl sqlx::PgExecutor<'_>,
    conversation_id: i64,
    usage: &crate::anthropic::TokenUsage,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO conversation_context_usage
             (conversation_id, input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, updated_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (conversation_id) DO UPDATE SET
             input_tokens = EXCLUDED.input_tokens,
             output_tokens = EXCLUDED.output_tokens,
             cache_creation_input_tokens = EXCLUDED.cache_creation_input_tokens,
             cache_read_input_tokens = EXCLUDED.cache_read_input_tokens,
             updated_at = now()",
    )
    .bind(conversation_id)
    .bind(usage.input_tokens)
    .bind(usage.output_tokens)
    .bind(usage.cache_creation_input_tokens)
    .bind(usage.cache_read_input_tokens)
    .execute(executor)
    .await?;
    Ok(())
}

/// Current todo list for `conversation_id` — empty if `todowrite` has never
/// been called for it. Mechanical mirror of `get_conversation_usage`
/// (flagged per docs/process/tdd.md's exception): same "last-known-only"
/// shape, just a JSONB list instead of four token counts. See
/// SME-20.
pub async fn get_conversation_todos(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<crate::anthropic::tools::TodoItem>, sqlx::Error> {
    let items = sqlx::query_scalar::<_, sqlx::types::Json<Vec<crate::anthropic::tools::TodoItem>>>(
        "SELECT items FROM conversation_todos WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;
    Ok(items.map(|sqlx::types::Json(v)| v).unwrap_or_default())
}

/// Overwrites `conversation_id`'s todo list with `items` — a full replace,
/// never a partial update (see SME-20's "no per-item ids" decision).
pub async fn set_conversation_todos(
    pool: &PgPool,
    conversation_id: i64,
    items: &[crate::anthropic::tools::TodoItem],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO conversation_todos (conversation_id, items, updated_at)
         VALUES ($1, $2, now())
         ON CONFLICT (conversation_id) DO UPDATE SET
             items = EXCLUDED.items,
             updated_at = now()",
    )
    .bind(conversation_id)
    .bind(sqlx::types::Json(items))
    .execute(pool)
    .await?;
    Ok(())
}

/// A conversation with a chosen id, for tests that touch process-wide,
/// per-conversation state (the turn lock, a stop, a pause): every
/// `#[sqlx::test]` database numbers conversations from 1, so tests running
/// in parallel would otherwise share that state through a common id.
#[cfg(test)]
pub async fn create_conversation_with_id(pool: &PgPool, id: i64) -> Result<Conversation, sqlx::Error> {
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (id, title) OVERRIDING SYSTEM VALUE VALUES ($1, 'New conversation') RETURNING *",
    )
    .bind(id)
    .fetch_one(pool)
    .await
}
