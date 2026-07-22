use std::sync::OnceLock;

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::models::{Conversation, Message};

static POOL: OnceLock<SqlitePool> = OnceLock::new();

pub async fn init() -> &'static SqlitePool {
    let db_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:./data/smelt.db".to_string());

    // `create_if_missing` below creates the database *file*, but not its
    // parent directory — a fresh checkout with no `data/` dir yet would
    // otherwise fail to connect at all.
    if let Some(path) = db_url.strip_prefix("sqlite:") {
        if let Some(parent) = std::path::Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).expect("failed to create database directory");
        }
    }

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(
            db_url
                .parse::<SqliteConnectOptions>()
                .expect("Invalid DATABASE_URL")
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .pragma("foreign_keys", "ON"),
        )
        .await
        .expect("Failed to connect to SQLite");

    POOL.set(pool).expect("Database already initialized");
    POOL.get().unwrap()
}

pub fn get() -> &'static SqlitePool {
    POOL.get()
        .expect("Database not initialized. Call db::init() first.")
}

const DEFAULT_TITLE: &str = "New Conversation";

pub async fn create_conversation() -> Result<Conversation, sqlx::Error> {
    sqlx::query_as::<_, Conversation>("INSERT INTO conversations (title) VALUES (?) RETURNING *")
        .bind(DEFAULT_TITLE)
        .fetch_one(get())
        .await
}

pub async fn list_conversations() -> Result<Vec<Conversation>, sqlx::Error> {
    sqlx::query_as::<_, Conversation>("SELECT * FROM conversations ORDER BY updated_at DESC")
        .fetch_all(get())
        .await
}

pub async fn list_messages(conversation_id: i64) -> Result<Vec<Message>, sqlx::Error> {
    sqlx::query_as::<_, Message>(
        "SELECT * FROM messages WHERE conversation_id = ? ORDER BY created_at ASC",
    )
    .bind(conversation_id)
    .fetch_all(get())
    .await
}

pub async fn create_message(
    conversation_id: i64,
    role: &str,
    content: &str,
) -> Result<Message, sqlx::Error> {
    let message = sqlx::query_as::<_, Message>(
        "INSERT INTO messages (conversation_id, role, content) VALUES (?, ?, ?) RETURNING *",
    )
    .bind(conversation_id)
    .bind(role)
    .bind(content)
    .fetch_one(get())
    .await?;

    // Bump updated_at so the sidebar can sort by recency; auto-title the
    // conversation from the first user message if it's still the default.
    sqlx::query(
        "UPDATE conversations
         SET updated_at = datetime('now'),
             title = CASE
                 WHEN title = ? AND ? = 'user' THEN substr(?, 1, 60)
                 ELSE title
             END
         WHERE id = ?",
    )
    .bind(DEFAULT_TITLE)
    .bind(role)
    .bind(content)
    .bind(conversation_id)
    .execute(get())
    .await?;

    Ok(message)
}

pub async fn delete_conversation(id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(id)
        .execute(get())
        .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use sqlx::{Connection, sqlite::SqliteConnection};
    use tokio::sync::OnceCell;

    static INIT: OnceCell<()> = OnceCell::const_new();

    /// Initialize a process-wide in-memory SQLite pool with migrations applied,
    /// stored in the same global `POOL` that `db::get()` reads. Idempotent and
    /// safe to call from every test; the first caller wins and the rest reuse it.
    pub async fn init_test_db() {
        INIT.get_or_init(|| async {
            // Each `#[tokio::test]` runs on its own runtime. A plain
            // `sqlite::memory:` database is private to a single connection, so
            // when a later test on a different runtime acquires a fresh
            // connection it sees an empty database ("no such table"). A *named,
            // shared-cache* in-memory database is shared by every connection
            // that opens the same URI and persists as long as one connection
            // stays open — so all tests, on any runtime, see the same migrated
            // schema.
            let opts = "sqlite:file:smelt_shared_test?mode=memory&cache=shared"
                .parse::<SqliteConnectOptions>()
                .expect("parse shared in-memory sqlite url")
                .create_if_missing(true)
                .pragma("foreign_keys", "ON");

            // A leaked keep-alive connection guarantees the shared in-memory DB
            // is never torn down (it vanishes once the last connection closes),
            // independent of the pool reaping idle connections.
            let keepalive = SqliteConnection::connect_with(&opts)
                .await
                .expect("open keep-alive connection to shared in-memory DB");
            std::mem::forget(keepalive);

            let pool = SqlitePoolOptions::new()
                .max_connections(5)
                .connect_with(opts)
                .await
                .expect("create shared in-memory test pool");
            sqlx::migrate!()
                .run(&pool)
                .await
                .expect("run migrations on test pool");
            POOL.set(pool).ok();
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::init_test_db;

    #[tokio::test]
    async fn test_create_and_list_conversations_round_trip() {
        init_test_db().await;

        let created = create_conversation().await.expect("create conversation");
        assert_eq!(created.title, DEFAULT_TITLE);

        let all = list_conversations().await.expect("list conversations");
        assert!(
            all.iter().any(|c| c.id == created.id),
            "created conversation should appear in list, got: {all:?}"
        );
    }

    #[tokio::test]
    async fn test_create_message_round_trips_and_lists_in_order() {
        init_test_db().await;

        let conversation = create_conversation().await.expect("create conversation");
        let first = create_message(conversation.id, "user", "hello")
            .await
            .expect("create first message");
        let second = create_message(conversation.id, "assistant", "hi there")
            .await
            .expect("create second message");

        let messages = list_messages(conversation.id).await.expect("list messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, first.id);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].id, second.id);
        assert_eq!(messages[1].role, "assistant");
    }

    #[tokio::test]
    async fn test_first_user_message_auto_titles_conversation() {
        init_test_db().await;

        let conversation = create_conversation().await.expect("create conversation");
        create_message(conversation.id, "user", "What's the weather like today?")
            .await
            .expect("create message");

        let all = list_conversations().await.expect("list conversations");
        let updated = all
            .iter()
            .find(|c| c.id == conversation.id)
            .expect("conversation should still exist");
        assert_eq!(updated.title, "What's the weather like today?");
    }

    #[tokio::test]
    async fn test_second_message_does_not_overwrite_title() {
        init_test_db().await;

        let conversation = create_conversation().await.expect("create conversation");
        create_message(conversation.id, "user", "first message")
            .await
            .expect("create first message");
        create_message(conversation.id, "assistant", "a reply")
            .await
            .expect("create assistant message");
        create_message(conversation.id, "user", "second message")
            .await
            .expect("create second message");

        let all = list_conversations().await.expect("list conversations");
        let updated = all
            .iter()
            .find(|c| c.id == conversation.id)
            .expect("conversation should still exist");
        assert_eq!(updated.title, "first message");
    }

    #[tokio::test]
    async fn test_delete_conversation_removes_it_from_list() {
        init_test_db().await;

        let conversation = create_conversation().await.expect("create conversation");
        delete_conversation(conversation.id)
            .await
            .expect("delete conversation");

        let all = list_conversations().await.expect("list conversations");
        assert!(
            !all.iter().any(|c| c.id == conversation.id),
            "deleted conversation should not appear in list, got: {all:?}"
        );
    }

    #[tokio::test]
    async fn test_delete_conversation_cascades_to_messages() {
        init_test_db().await;

        let conversation = create_conversation().await.expect("create conversation");
        create_message(conversation.id, "user", "hello")
            .await
            .expect("create message");

        delete_conversation(conversation.id)
            .await
            .expect("delete conversation");

        let messages = list_messages(conversation.id).await.expect("list messages");
        assert!(
            messages.is_empty(),
            "messages should be cascade-deleted with their conversation, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn test_delete_nonexistent_conversation_is_a_no_op() {
        init_test_db().await;

        delete_conversation(-1)
            .await
            .expect("deleting a nonexistent conversation should not error");
    }
}
