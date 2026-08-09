use std::sync::OnceLock;
use std::time::Duration;

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::anthropic::ContentBlock;
use crate::models::{Conversation, Message};

static POOL: OnceLock<PgPool> = OnceLock::new();

const CONNECT_RETRIES: u32 = 10;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

pub async fn init() -> &'static PgPool {
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let options = db_url
        .parse::<PgConnectOptions>()
        .expect("Invalid DATABASE_URL");

    // A freshly-started container doesn't mean Postgres is accepting
    // connections yet — especially on first boot, while it initializes its
    // data directory. A bounded retry loop tolerates that startup window
    // without hanging forever if Postgres is genuinely misconfigured.
    let mut attempt = 0;
    let pool = loop {
        attempt += 1;
        match PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options.clone())
            .await
        {
            Ok(pool) => break pool,
            Err(err) if attempt < CONNECT_RETRIES => {
                tracing::warn!(
                    "failed to connect to Postgres (attempt {attempt}/{CONNECT_RETRIES}): {err}"
                );
                tokio::time::sleep(CONNECT_RETRY_DELAY).await;
            }
            Err(err) => {
                panic!("Failed to connect to Postgres after {CONNECT_RETRIES} attempts: {err}")
            }
        }
    };

    POOL.set(pool).expect("Database already initialized");
    POOL.get().unwrap()
}

pub fn get() -> &'static PgPool {
    POOL.get()
        .expect("Database not initialized. Call db::init() first.")
}

const DEFAULT_TITLE: &str = "New Conversation";

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
    let content_json = serde_json::to_string(content)
        .expect("ContentBlock always serializes: no non-string map keys, no floats");

    let message = sqlx::query_as::<_, Message>(
        "INSERT INTO messages (conversation_id, role, content) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(conversation_id)
    .bind(role)
    .bind(&content_json)
    .fetch_one(pool)
    .await?;

    // Bump updated_at so the sidebar can sort by recency; auto-title the
    // conversation from the first user message if it's still the default.
    sqlx::query(
        "UPDATE conversations
         SET updated_at = now(),
             title = CASE
                 WHEN title = $1 AND $2 = 'user' THEN left($3, 60)
                 ELSE title
             END
         WHERE id = $4",
    )
    .bind(DEFAULT_TITLE)
    .bind(role)
    .bind(title_candidate(content))
    .bind(conversation_id)
    .execute(pool)
    .await?;

    Ok(message)
}

pub async fn delete_conversation(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn test_create_and_list_conversations_round_trip(pool: PgPool) {
        let created = create_conversation(&pool)
            .await
            .expect("create conversation");
        assert_eq!(created.title, DEFAULT_TITLE);

        let all = list_conversations(&pool).await.expect("list conversations");
        assert!(
            all.iter().any(|c| c.id == created.id),
            "created conversation should appear in list, got: {all:?}"
        );
    }

    #[sqlx::test]
    async fn test_create_message_round_trips_and_lists_in_order(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        let first = create_message(
            &pool,
            conversation.id,
            "user",
            &[ContentBlock::Text {
                text: "hello".to_string(),
            }],
        )
        .await
        .expect("create first message");
        let second = create_message(
            &pool,
            conversation.id,
            "assistant",
            &[ContentBlock::Text {
                text: "hi there".to_string(),
            }],
        )
        .await
        .expect("create second message");

        let messages = list_messages(&pool, conversation.id)
            .await
            .expect("list messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, first.id);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].id, second.id);
        assert_eq!(messages[1].role, "assistant");
    }

    #[sqlx::test]
    async fn test_create_message_stores_content_as_json_blocks(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        let blocks = vec![
            ContentBlock::ToolUse {
                id: "toolu_01".to_string(),
                name: "add".to_string(),
                input: serde_json::json!({"a": 1, "b": 2}),
            },
            ContentBlock::Text {
                text: "done".to_string(),
            },
        ];
        let saved = create_message(&pool, conversation.id, "assistant", &blocks)
            .await
            .expect("create message");

        assert_eq!(
            saved
                .blocks()
                .expect("stored content should parse as blocks"),
            blocks
        );
    }

    #[sqlx::test]
    async fn test_first_user_message_auto_titles_conversation(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        create_message(
            &pool,
            conversation.id,
            "user",
            &[ContentBlock::Text {
                text: "What's the weather like today?".to_string(),
            }],
        )
        .await
        .expect("create message");

        let all = list_conversations(&pool).await.expect("list conversations");
        let updated = all
            .iter()
            .find(|c| c.id == conversation.id)
            .expect("conversation should still exist");
        assert_eq!(updated.title, "What's the weather like today?");
    }

    #[sqlx::test]
    async fn test_second_message_does_not_overwrite_title(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        create_message(
            &pool,
            conversation.id,
            "user",
            &[ContentBlock::Text {
                text: "first message".to_string(),
            }],
        )
        .await
        .expect("create first message");
        create_message(
            &pool,
            conversation.id,
            "assistant",
            &[ContentBlock::Text {
                text: "a reply".to_string(),
            }],
        )
        .await
        .expect("create assistant message");
        create_message(
            &pool,
            conversation.id,
            "user",
            &[ContentBlock::Text {
                text: "second message".to_string(),
            }],
        )
        .await
        .expect("create second message");

        let all = list_conversations(&pool).await.expect("list conversations");
        let updated = all
            .iter()
            .find(|c| c.id == conversation.id)
            .expect("conversation should still exist");
        assert_eq!(updated.title, "first message");
    }

    #[sqlx::test]
    async fn test_delete_conversation_removes_it_from_list(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        delete_conversation(&pool, conversation.id)
            .await
            .expect("delete conversation");

        let all = list_conversations(&pool).await.expect("list conversations");
        assert!(
            !all.iter().any(|c| c.id == conversation.id),
            "deleted conversation should not appear in list, got: {all:?}"
        );
    }

    #[sqlx::test]
    async fn test_delete_conversation_cascades_to_messages(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        create_message(
            &pool,
            conversation.id,
            "user",
            &[ContentBlock::Text {
                text: "hello".to_string(),
            }],
        )
        .await
        .expect("create message");

        delete_conversation(&pool, conversation.id)
            .await
            .expect("delete conversation");

        let messages = list_messages(&pool, conversation.id)
            .await
            .expect("list messages");
        assert!(
            messages.is_empty(),
            "messages should be cascade-deleted with their conversation, got: {messages:?}"
        );
    }

    #[sqlx::test]
    async fn test_delete_nonexistent_conversation_is_a_no_op(pool: PgPool) {
        delete_conversation(&pool, -1)
            .await
            .expect("deleting a nonexistent conversation should not error");
    }
}
