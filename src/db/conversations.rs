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

/// The longest title a conversation gets from its first message.
pub(super) const TITLE_CHARS: usize = 60;

/// The titles a first message suggests, for the user's own conversation
/// and for an agent's: its `Text` blocks (`ToolUse`/`ToolResult` blocks
/// carry nothing sensible to show), read by `title_from_text`.
fn title_candidates(content: &[ContentBlock]) -> (String, String) {
    let text = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    (title_from_text(&text, false), title_from_text(&text, true))
}

/// A short, plain title for a message (SME-123 DR1): its first non-blank
/// line, markdown marks dropped, whitespace collapsed, and cut at a word
/// with an ellipsis past `TITLE_CHARS`. For an agent's brief (`brief`),
/// which opens "You are the **Ticket Driver for SME-345** (Linear, …" as
/// every brief does, the line's first bold phrase, the part that tells two
/// apart. Not for the user's own message: "Fix the **login** page" isn't
/// titled "login" (review 1 M2).
pub fn title_from_text(text: &str, brief: bool) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let chosen = if brief { bold_phrase(line).unwrap_or(line) } else { line };
    let plain: String = chosen
        .trim_start_matches(['#', '>', ' '])
        .replace("**", "")
        .replace("__", "")
        .replace('`', "");
    let words: Vec<&str> = plain.split_whitespace().collect();
    let mut title = String::new();
    for word in &words {
        let next = if title.is_empty() { word.chars().count() } else { title.chars().count() + 1 + word.chars().count() };
        if next > TITLE_CHARS {
            if title.is_empty() {
                // One word longer than a title: cut it.
                title = word.chars().take(TITLE_CHARS - 1).collect();
            }
            title.push('\u{2026}');
            return title;
        }
        if !title.is_empty() {
            title.push(' ');
        }
        title.push_str(word);
    }
    title
}

/// The text of the first `**bold**` span in `line`, if it has one with
/// something in it.
fn bold_phrase(line: &str) -> Option<&str> {
    let start = line.find("**")? + 2;
    let end = start + line[start..].find("**")?;
    let phrase = line[start..end].trim();
    (!phrase.is_empty()).then_some(phrase)
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
    let message = insert_message_on(&mut *conn, conversation_id, role, content).await?;
    let (own_title, brief_title) = title_candidates(content);

    // Bump updated_at so the sidebar can sort by recency; auto-title the
    // conversation from the first user message if it's still the default (in
// either spelling: before SME-41 the default was "New Conversation").
    sqlx::query(
        "UPDATE conversations
         SET updated_at = now(),
             title = CASE
                 WHEN lower(title) <> lower($1) OR $2 <> 'user' THEN title
                 WHEN parent_id IS NOT NULL AND $5 <> '' THEN $5
                 WHEN $3 <> '' THEN $3
                 ELSE title
             END
         WHERE id = $4",
    )
    .bind(DEFAULT_TITLE)
    .bind(role)
    .bind(&own_title)
    .bind(conversation_id)
    .bind(&brief_title)
    .execute(&mut *conn)
    .await?;

    Ok(message)
}

/// Saves a message without touching its conversation's row: `updated_at`
/// stays, so the sidebar doesn't re-sort, and a default title isn't
/// replaced by the message's text. For smelt's quiet notices, which aren't
/// news (SME-78: a pod stopped on idle or at an agent's finish).
pub async fn create_message_in_place(
    pool: &PgPool,
    conversation_id: i64,
    role: &str,
    content: &[ContentBlock],
) -> Result<Message, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    insert_message_on(&mut conn, conversation_id, role, content).await
}

/// The `messages` insert alone, shared by `create_message_on` and
/// `create_message_in_place`.
async fn insert_message_on(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    role: &str,
    content: &[ContentBlock],
) -> Result<Message, sqlx::Error> {
    #[expect(clippy::expect_used, reason = "a ContentBlock always serializes: its map keys are strings, and a serde_json::Value holds only finite numbers")]
    let content_json = serde_json::to_string(content)
        .expect("ContentBlock always serializes: no non-string map keys, no floats");
    sqlx::query_as::<_, Message>(
        "INSERT INTO messages (conversation_id, role, content) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(conversation_id)
    .bind(role)
    .bind(&content_json)
    .fetch_one(conn)
    .await
}

pub async fn conversation_exists(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM conversations WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// The conversation's title, or `None` when there is no such conversation
/// (SME-71: a notification names its conversation, and this is also the
/// attention endpoint's "the conversation is gone" answer).
pub async fn get_conversation_title(pool: &PgPool, id: i64) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT title FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
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

/// The conversation's parent, and whether it has children of its own: who
/// may call `message_agent` (SME-238; it's offered to everyone). `(None, false)` for one that
/// doesn't exist.
pub async fn conversation_links(pool: &PgPool, id: i64) -> Result<(Option<i64>, bool), sqlx::Error> {
    Ok(sqlx::query_as::<_, (Option<i64>, bool)>(
        "SELECT parent_id, EXISTS (SELECT 1 FROM conversations k WHERE k.parent_id = c.id) FROM conversations c WHERE c.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .unwrap_or((None, false)))
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

#[cfg(test)]
mod title_tests {
    use super::{TITLE_CHARS, title_from_text};

    #[test]
    fn test_an_agent_brief_is_titled_by_its_first_bold_phrase() {
        let brief = "You are the **Ticket Driver for SME-345** (Linear, team Smelt Agent): **Remove volumes**.\n\nMore.";
        assert_eq!(title_from_text(brief, true), "Ticket Driver for SME-345");
    }

    #[test]
    fn test_without_bold_the_first_line_is_the_title_with_its_marks_dropped() {
        assert_eq!(title_from_text("\n\n## Fix the `login` page\nsecond line", true), "Fix the login page");
        assert_eq!(title_from_text("hello   there", false), "hello there");
    }

    #[test]
    fn test_a_long_title_is_cut_at_a_word_with_an_ellipsis() {
        let title = title_from_text("Following the development process in the repo, start working on the ticket now please", false);
        assert!(title.ends_with('\u{2026}'), "{title}");
        assert!(title.chars().count() <= TITLE_CHARS + 1, "{title}");
        assert!(title.starts_with("Following the development process in the repo, start"), "cut at a word: {title}");
        let one_word = title_from_text(&"x".repeat(100), false);
        assert_eq!(one_word.chars().count(), TITLE_CHARS);
    }

    #[test]
    fn test_the_users_own_message_keeps_its_line_however_it_bolds() {
        assert_eq!(title_from_text("Fix the **login** page bug", false), "Fix the login page bug");
        assert_eq!(title_from_text("Fix the **login** page bug", true), "login");
    }

    #[test]
    fn test_an_unclosed_or_empty_bold_falls_back_to_the_line() {
        assert_eq!(title_from_text("a **b", true), "a b");
        assert_eq!(title_from_text("a **** b", true), "a b");
    }
}

/// The conversation's `pod_resources` (SME-484): what its pods ask of
/// their node, as JSON; `None` for the deployment's defaults, and for no
/// such conversation.
pub async fn conversation_pod_resources(pool: &PgPool, id: i64) -> Result<Option<serde_json::Value>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<serde_json::Value>>("SELECT pod_resources FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(Option::flatten)
}

/// Sets (or, with `None`, clears) the conversation's `pod_resources`.
pub async fn set_conversation_pod_resources(pool: &PgPool, id: i64, resources: Option<&serde_json::Value>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversations SET pod_resources = $2 WHERE id = $1")
        .bind(id)
        .bind(resources)
        .execute(pool)
        .await
        .map(|_| ())
}
