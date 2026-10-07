use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
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

const DEFAULT_TITLE: &str = "New conversation";

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
async fn create_message_on(
    conn: &mut sqlx::PgConnection,
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
/// it describe a history that's no longer sent (SME-51 B10).
pub async fn clear_conversation_usage(pool: &PgPool, conversation_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversation_context_usage WHERE conversation_id = $1")
        .bind(conversation_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Overwrites `conversation_id`'s usage row with `usage` — "last known
/// only," not a history, so this is always a full replace, not an
/// accumulation.
pub async fn upsert_conversation_usage(
    pool: &PgPool,
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
    .execute(pool)
    .await?;
    Ok(())
}

/// Current todo list for `conversation_id` — empty if `todowrite` has never
/// been called for it. Mechanical mirror of `get_conversation_usage`
/// (flagged per development-process.md's exception): same "last-known-only"
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

// --- Pending questions (the model's ask_user, SME-34) ---

/// A conversation's `pending_questions` row: the question it waits on, and
/// the user's answer once given (`None` while waiting).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredQuestion {
    pub tool_use_id: String,
    pub questions: Vec<crate::questions::Question>,
    pub answer: Option<Vec<crate::questions::QuestionAnswer>>,
}

/// Records the question `conversation_id` now waits on, replacing any
/// earlier one (there is none: a turn only runs once the last is taken).
pub async fn create_pending_question(
    pool: &PgPool,
    conversation_id: i64,
    tool_use_id: &str,
    questions: &[crate::questions::Question],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pending_questions (conversation_id, tool_use_id, questions)
         VALUES ($1, $2, $3)
         ON CONFLICT (conversation_id) DO UPDATE SET
             tool_use_id = EXCLUDED.tool_use_id,
             questions = EXCLUDED.questions,
             answer = NULL,
             created_at = now()",
    )
    .bind(conversation_id)
    .bind(tool_use_id)
    .bind(sqlx::types::Json(questions))
    .execute(pool)
    .await?;
    Ok(())
}

type StoredQuestionRow = (
    String,
    sqlx::types::Json<Vec<crate::questions::Question>>,
    Option<sqlx::types::Json<Vec<crate::questions::QuestionAnswer>>>,
);

fn stored_question((tool_use_id, questions, answer): StoredQuestionRow) -> StoredQuestion {
    StoredQuestion {
        tool_use_id,
        questions: questions.0,
        answer: answer.map(|a| a.0),
    }
}

/// The question `conversation_id` waits on or has an answer for, if any.
pub async fn get_pending_question(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<StoredQuestion>, sqlx::Error> {
    let row = sqlx::query_as::<_, StoredQuestionRow>(
        "SELECT tool_use_id, questions, answer FROM pending_questions WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(stored_question))
}

/// Records `answer` for the waiting question `tool_use_id`. False when
/// there's no such question or it was already answered (another tab got
/// there first): exactly one answer is ever recorded.
pub async fn answer_pending_question(
    pool: &PgPool,
    conversation_id: i64,
    tool_use_id: &str,
    answer: &[crate::questions::QuestionAnswer],
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE pending_questions SET answer = $3
         WHERE conversation_id = $1 AND tool_use_id = $2 AND answer IS NULL",
    )
    .bind(conversation_id)
    .bind(tool_use_id)
    .bind(sqlx::types::Json(answer))
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Removes and returns `conversation_id`'s question if it has an answer,
/// or (`include_waiting`) if it's still waiting. A turn does this through
/// `create_message_taking_question`, in the transaction that saves the
/// call's result; the tests check the rule on its own here.
#[cfg(test)]
pub async fn take_pending_question(
    pool: &PgPool,
    conversation_id: i64,
    include_waiting: bool,
) -> Result<Option<StoredQuestion>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    take_pending_question_on(&mut conn, conversation_id, include_waiting).await
}

async fn take_pending_question_on(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    include_waiting: bool,
) -> Result<Option<StoredQuestion>, sqlx::Error> {
    let row = sqlx::query_as::<_, StoredQuestionRow>(
        "DELETE FROM pending_questions
         WHERE conversation_id = $1 AND (answer IS NOT NULL OR $2)
         RETURNING tool_use_id, questions, answer",
    )
    .bind(conversation_id)
    .bind(include_waiting)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(stored_question))
}

/// Saves a turn's message (`content`, empty for a wake with nothing of its
/// own) after taking the conversation's question as `take_pending_question`
/// does, with `result_for(question)` put first: both in one transaction, so
/// an answer is never lost (taken, message not saved) or sent twice (saved,
/// row left). `None` when there's nothing to save.
pub async fn create_message_taking_question(
    pool: &PgPool,
    conversation_id: i64,
    role: &str,
    content: Vec<ContentBlock>,
    include_waiting: bool,
    result_for: impl FnOnce(&StoredQuestion) -> ContentBlock,
) -> Result<Option<(Message, Option<StoredQuestion>)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let question = take_pending_question_on(&mut tx, conversation_id, include_waiting).await?;
    let mut blocks = Vec::with_capacity(content.len() + 1);
    blocks.extend(question.as_ref().map(result_for));
    blocks.extend(content);
    if blocks.is_empty() {
        tx.rollback().await?;
        return Ok(None);
    }
    let saved = create_message_on(&mut tx, conversation_id, role, &blocks).await?;
    tx.commit().await?;
    Ok(Some((saved, question)))
}

/// Conversations with a question still waiting for an answer, for the
/// sidebar's waiting mark.
pub async fn list_waiting_conversations(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT conversation_id FROM pending_questions WHERE answer IS NULL ORDER BY conversation_id")
        .fetch_all(pool)
        .await
}

// --- Terminal (pod/terminal/command lifecycle) ---
// Server-only, no client/server boundary to cross (no UI yet) — unlike
// Conversation/Message, these don't need to live in models.rs or derive
// anything WASM-relevant. See SME-9.

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct SandboxPod {
    pub id: i64,
    pub conversation_id: i64,
    pub created_at: NaiveDateTime,
    pub terminated_at: Option<NaiveDateTime>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct SandboxTerminal {
    pub id: i64,
    pub pod_id: i64,
    pub created_at: NaiveDateTime,
    pub terminated_at: Option<NaiveDateTime>,
}

pub async fn create_sandbox_pod(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<SandboxPod, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "INSERT INTO sandbox_pods (conversation_id) VALUES ($1) RETURNING *",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
}

/// Idempotent by design (unlike creation) — terminating an already-
/// terminated pod converges on the same end state, so it's safe to call
/// without first checking. A `pod_id` that doesn't exist at all is a
/// separate, real error, surfaced by the caller checking the row count
/// via `RETURNING` returning nothing — see `sandbox.rs`.
pub async fn terminate_sandbox_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<SandboxPod>, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(pod_id)
    .fetch_optional(pool)
    .await
}

/// One live pod, across every conversation, with what the pods view needs
/// from the database: its conversation, and its terminals' activity.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct LivePodRow {
    pub pod_id: i64,
    pub conversation_id: i64,
    pub conversation_title: String,
    /// Bumped by every message, so it stands in for tool activity that
    /// leaves no per-pod record.
    pub conversation_updated_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
    pub live_terminals: i64,
    pub running_commands: i64,
    pub last_command_finished_at: Option<NaiveDateTime>,
    /// The database's own `now()`, so ages are worked out on one clock.
    pub observed_at: NaiveDateTime,
}

/// Every live pod, oldest first — the pods view's rows.
pub async fn list_live_pods(pool: &PgPool) -> Result<Vec<LivePodRow>, sqlx::Error> {
    sqlx::query_as::<_, LivePodRow>(
        "SELECT p.id AS pod_id,
                p.conversation_id,
                c.title AS conversation_title,
                c.updated_at AS conversation_updated_at,
                p.created_at,
                (SELECT COUNT(*) FROM sandbox_terminals t
                  WHERE t.pod_id = p.id AND t.terminated_at IS NULL) AS live_terminals,
                (SELECT COUNT(*) FROM terminal_commands tc
                   JOIN sandbox_terminals t ON t.id = tc.terminal_id
                  WHERE t.pod_id = p.id AND tc.status = 'running') AS running_commands,
                (SELECT MAX(tc.finished_at) FROM terminal_commands tc
                   JOIN sandbox_terminals t ON t.id = tc.terminal_id
                  WHERE t.pod_id = p.id) AS last_command_finished_at,
                now()::timestamp AS observed_at
           FROM sandbox_pods p
           JOIN conversations c ON c.id = p.conversation_id
          WHERE p.terminated_at IS NULL
          ORDER BY p.created_at ASC, p.id ASC",
    )
    .fetch_all(pool)
    .await
}

/// Live pods created more than `min_age_secs` seconds ago, by the
/// database's clock — the ones old enough to have a pod in Kubernetes.
pub async fn live_pods_older_than(pool: &PgPool, min_age_secs: i64) -> Result<Vec<SandboxPod>, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "SELECT * FROM sandbox_pods
          WHERE terminated_at IS NULL AND created_at < now() - make_interval(secs => $1)
          ORDER BY id ASC",
    )
    .bind(min_age_secs as f64)
    .fetch_all(pool)
    .await
}

/// Whether `pod_id` exists and hasn't been terminated.
pub async fn sandbox_pod_is_live(pool: &PgPool, pod_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sandbox_pods WHERE id = $1 AND terminated_at IS NULL)",
    )
    .bind(pod_id)
    .fetch_one(pool)
    .await
}

/// The conversations that have a live pod — the sidebar's markers.
pub async fn conversations_with_live_pods(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT conversation_id FROM sandbox_pods
          WHERE terminated_at IS NULL ORDER BY conversation_id ASC",
    )
    .fetch_all(pool)
    .await
}

/// Live pods only (`terminated_at IS NULL`) — a terminated pod's row
/// sticks around (see SME-9's "How") but shouldn't be listed as if it
/// still existed.
pub async fn list_sandbox_pods(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<SandboxPod>, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "SELECT * FROM sandbox_pods WHERE conversation_id = $1 AND terminated_at IS NULL ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

pub async fn create_sandbox_terminal(
    pool: &PgPool,
    pod_id: i64,
) -> Result<SandboxTerminal, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "INSERT INTO sandbox_terminals (pod_id) VALUES ($1) RETURNING *",
    )
    .bind(pod_id)
    .fetch_one(pool)
    .await
}

/// Same idempotent-on-repeat, real-error-on-unknown-id shape as
/// `terminate_sandbox_pod`.
pub async fn terminate_sandbox_terminal(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<SandboxTerminal>, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "UPDATE sandbox_terminals SET terminated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(terminal_id)
    .fetch_optional(pool)
    .await
}

/// Live terminals for one pod — backs `terminate_pod`'s guard (refuse if
/// non-empty).
pub async fn list_sandbox_terminals_for_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Vec<SandboxTerminal>, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "SELECT * FROM sandbox_terminals WHERE pod_id = $1 AND terminated_at IS NULL ORDER BY id ASC",
    )
    .bind(pod_id)
    .fetch_all(pool)
    .await
}

/// Live terminals across every pod in a conversation — backs
/// `list_terminals`, which shows the model everything it has regardless
/// of which pod it's in.
pub async fn list_sandbox_terminals_for_conversation(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<SandboxTerminal>, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>(
        "SELECT st.* FROM sandbox_terminals st
         JOIN sandbox_pods sp ON sp.id = st.pod_id
         WHERE sp.conversation_id = $1 AND st.terminated_at IS NULL
         ORDER BY st.id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// The owning pod for a terminal — `sandbox.rs` needs this before it can
/// look up (or establish) that pod's agent connection.
pub async fn sandbox_terminal_pod_id(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT pod_id FROM sandbox_terminals WHERE id = $1")
        .bind(terminal_id)
        .fetch_optional(pool)
        .await
}

/// The conversation whose pod `terminal_id` is in, if it exists: the tools
/// only act on the calling conversation's terminals (SME-51 B2).
pub async fn terminal_conversation_id(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT p.conversation_id FROM sandbox_terminals t
           JOIN sandbox_pods p ON p.id = t.pod_id
          WHERE t.id = $1",
    )
    .bind(terminal_id)
    .fetch_optional(pool)
    .await
}

/// Resolves which conversation's `events::publish` bus a pod-scoped call
/// (`terminate_pod`, `create_terminal`, `terminate_terminal`, crash
/// cleanup) should target — see
/// SME-10.
pub async fn sandbox_pod_conversation_id(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT conversation_id FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_optional(pool)
        .await
}

/// Records `host:port` as a preview the model shared for `pod_id` (SME-42)
/// and returns every preview shared for that pod so far, as
/// `list_pod_previews` orders them. `host` is `""` for the pod's own
/// localhost, or a Docker container's address (SME-33). Sharing one twice
/// is fine.
pub async fn add_pod_preview(
    pool: &PgPool,
    pod_id: i64,
    host: &str,
    port: u16,
) -> Result<Vec<(String, u16)>, sqlx::Error> {
    sqlx::query("INSERT INTO sandbox_pod_previews (pod_id, host, port) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(pod_id)
        .bind(host)
        .bind(i32::from(port))
        .execute(pool)
        .await?;
    list_pod_previews(pool, pod_id).await
}

/// Every preview shared for `pod_id` as `(host, port)`: the pod's own
/// localhost (`""`) first, then by container address, each lowest port first.
pub async fn list_pod_previews(pool: &PgPool, pod_id: i64) -> Result<Vec<(String, u16)>, sqlx::Error> {
    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT host, port FROM sandbox_pod_previews WHERE pod_id = $1 ORDER BY host, port",
    )
    .bind(pod_id)
    .fetch_all(pool)
    .await?;
    // The table's CHECK keeps every port in u16's range.
    Ok(rows
        .into_iter()
        .filter_map(|(host, port)| Some((host, u16::try_from(port).ok()?)))
        .collect())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct TerminalCommand {
    pub id: i64,
    pub conversation_id: i64,
    pub terminal_id: i64,
    pub command_id: String,
    pub command: String,
    /// "running" | "finished" | "lost" — enforced by the table's CHECK
    /// constraint, not re-validated here.
    pub status: String,
    pub exit_code: Option<i32>,
    pub notified_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub finished_at: Option<NaiveDateTime>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct TerminalCommandStatus {
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout_lines: i64,
    pub stderr_lines: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct TerminalLine {
    pub stream: String,
    pub data: String,
    /// Global order across *both* streams for one command — a caller that
    /// fetches stdout and stderr as two separate calls (e.g. to cap each
    /// stream's tail independently, see `api::sandbox::fetch_command_summary`)
    /// needs this to merge them back into the order they actually
    /// happened in, rather than showing "all stdout, then all stderr."
    pub seq: i64,
}

pub async fn create_terminal_command(
    pool: &PgPool,
    conversation_id: i64,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> Result<TerminalCommand, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "INSERT INTO terminal_commands (conversation_id, terminal_id, command_id, command, status)
         VALUES ($1, $2, $3, $4, 'running') RETURNING *",
    )
    .bind(conversation_id)
    .bind(terminal_id)
    .bind(command_id)
    .bind(command)
    .fetch_one(pool)
    .await
}

/// Backs the single-command-in-flight check `run_terminal_command` and
/// `terminate_terminal` both need — at most one row per terminal can
/// ever be `running` at a time (one in-flight command per terminal, not
/// per conversation — see SME-9's "What"), enforced at the tool layer,
/// not by a database constraint.
pub async fn terminal_command_is_running(
    pool: &PgPool,
    terminal_id: i64,
) -> Result<Option<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "SELECT * FROM terminal_commands WHERE terminal_id = $1 AND status = 'running'",
    )
    .bind(terminal_id)
    .fetch_optional(pool)
    .await
}

pub async fn append_terminal_event(
    pool: &PgPool,
    command_id: &str,
    stream: &str,
    seq: i64,
    data: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO terminal_events (command_id, stream, seq, data) VALUES ($1, $2, $3, $4)",
    )
    .bind(command_id)
    .bind(stream)
    .bind(seq)
    .bind(data)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_terminal_command_finished(
    pool: &PgPool,
    command_id: &str,
    exit_code: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE terminal_commands SET status = 'finished', exit_code = $2, finished_at = now()
         WHERE command_id = $1",
    )
    .bind(command_id)
    .bind(exit_code)
    .execute(pool)
    .await?;
    Ok(())
}

/// Used only by the crash-recovery path: a command that was `running` when
/// its agent was found unreachable has no real exit code to report, so
/// `exit_code` stays `NULL` — see SME-9's "Agent crash recovery."
/// Restricted to rows still `running` so this is safe to call defensively
/// without first checking status.
pub async fn mark_terminal_command_lost(
    pool: &PgPool,
    command_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE terminal_commands SET status = 'lost', finished_at = now()
         WHERE command_id = $1 AND status = 'running'",
    )
    .bind(command_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `send_signal` addresses a command purely by its (globally-unique)
/// `command_id`, same as `terminal_command_status`/`read_terminal_output`
/// — but `sandbox::send_signal` still needs `terminal_id` to know which
/// pod to reach, hence this lookup.
pub async fn get_terminal_command(
    pool: &PgPool,
    command_id: &str,
) -> Result<Option<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>("SELECT * FROM terminal_commands WHERE command_id = $1")
        .bind(command_id)
        .fetch_optional(pool)
        .await
}

/// A snapshot query, not a wait — `terminal_command_status` never blocks.
/// Line counts come from `terminal_events` via a `LEFT JOIN` so a command
/// with zero output of a given stream still returns `0`, not `NULL`.
pub async fn terminal_command_status(
    pool: &PgPool,
    command_id: &str,
) -> Result<Option<TerminalCommandStatus>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommandStatus>(
        "SELECT tc.status, tc.exit_code,
                COALESCE(SUM(CASE WHEN te.stream = 'stdout' THEN 1 ELSE 0 END), 0) AS stdout_lines,
                COALESCE(SUM(CASE WHEN te.stream = 'stderr' THEN 1 ELSE 0 END), 0) AS stderr_lines
         FROM terminal_commands tc
         LEFT JOIN terminal_events te ON te.command_id = tc.command_id
         WHERE tc.command_id = $1
         GROUP BY tc.status, tc.exit_code",
    )
    .bind(command_id)
    .fetch_optional(pool)
    .await
}

/// `LIMIT`/`OFFSET` over the requested stream(s), ordered by the agent-
/// assigned `seq` — *not* `id` (see SME-9's "Ordering" section on why
/// insertion order isn't trusted as the real order). `streams` is typically
/// `&["stdout"]`, `&["stderr"]`, or `&["stdout", "stderr"]` — line numbers
/// are relative to whichever set is requested, by design.
pub async fn read_terminal_output(
    pool: &PgPool,
    command_id: &str,
    streams: &[&str],
    offset: i64,
    limit: i64,
) -> Result<Vec<TerminalLine>, sqlx::Error> {
    sqlx::query_as::<_, TerminalLine>(
        "SELECT stream, data, seq FROM terminal_events
         WHERE command_id = $1 AND stream = ANY($2)
         ORDER BY seq ASC
         OFFSET $3 LIMIT $4",
    )
    .bind(command_id)
    .bind(streams)
    .bind(offset)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Backs the completion-notification check in `run_turn`'s loop — both
/// terminal states (`finished` and `lost`) need exactly one notification
/// each, so this matches either, not just `finished`.
pub async fn unnotified_finished_terminal_commands(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "SELECT * FROM terminal_commands
         WHERE conversation_id = $1 AND status IN ('finished', 'lost') AND notified_at IS NULL
         ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// Only tests mark a command without its notice; a turn uses
/// `save_command_notice`, which does both at once.
#[cfg(test)]
pub async fn mark_terminal_command_notified(
    pool: &PgPool,
    command_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE terminal_commands SET notified_at = now() WHERE command_id = $1")
        .bind(command_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Saves `content` as `command_id`'s notice in `conversation_id` and marks
/// the command notified, in one transaction: a turn stopped part-way
/// leaves both or neither, so the next drain can't save the notice again
/// (SME-91). `None`, with nothing saved, when the command was already
/// notified or doesn't exist.
pub async fn save_command_notice(
    pool: &PgPool,
    conversation_id: i64,
    command_id: &str,
    content: &[ContentBlock],
) -> Result<Option<Message>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let marked = sqlx::query(
        "UPDATE terminal_commands SET notified_at = now()
         WHERE command_id = $1 AND notified_at IS NULL",
    )
    .bind(command_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if marked == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    let saved = create_message_on(&mut tx, conversation_id, "user", content).await?;
    tx.commit().await?;
    Ok(Some(saved))
}

/// Backs `list_commands` — most-recent-first, bounded the same way
/// `read_terminal_output` is, scoped to one terminal (its history
/// outlives that terminal being torn down, since `terminal_commands`
/// isn't cascade-deleted by `terminate_terminal` — see SME-9's "How").
pub async fn list_terminal_commands(
    pool: &PgPool,
    terminal_id: i64,
    limit: i64,
) -> Result<Vec<TerminalCommand>, sqlx::Error> {
    sqlx::query_as::<_, TerminalCommand>(
        "SELECT * FROM terminal_commands WHERE terminal_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(terminal_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

// --- Sandbox volumes (generic pod mounts, configured via the
// /sandbox-volumes UI) --- Plain CRUD, no soft delete — configuration a
// person edits, same precedent as mcp_servers below, not a live external
// resource like a sandbox pod itself. See
// SME-17's Phase 4.

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct SandboxVolume {
    pub id: i64,
    pub name: String,
    /// Already-resolved (any leading `~` expanded to the sandbox user's
    /// home directory before this row is ever written) — a plain absolute
    /// path, set once at creation and never changed after.
    pub mount_path: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

pub async fn create_sandbox_volume(
    pool: &PgPool,
    name: &str,
    mount_path: &str,
) -> Result<SandboxVolume, sqlx::Error> {
    sqlx::query_as::<_, SandboxVolume>(
        "INSERT INTO sandbox_volumes (name, mount_path) VALUES ($1, $2) RETURNING *",
    )
    .bind(name)
    .bind(mount_path)
    .fetch_one(pool)
    .await
}

/// Every configured volume — unlike `list_sandbox_pods`, not scoped to a
/// conversation at all: a volume is defined once and every pod smelt
/// creates gets every one of these mounted, per the idea's "not something
/// chosen per conversation."
pub async fn list_sandbox_volumes(pool: &PgPool) -> Result<Vec<SandboxVolume>, sqlx::Error> {
    sqlx::query_as::<_, SandboxVolume>("SELECT * FROM sandbox_volumes ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn get_sandbox_volume(
    pool: &PgPool,
    id: i64,
) -> Result<Option<SandboxVolume>, sqlx::Error> {
    sqlx::query_as::<_, SandboxVolume>("SELECT * FROM sandbox_volumes WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn delete_sandbox_volume(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sandbox_volumes WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// --- Git: SSH keys and the commit identity (SME-32) ---

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct SshKey {
    pub id: i64,
    pub name: String,
    pub public_key: String,
    /// Plain text for now: see SME-48.
    pub private_key: String,
    pub created_at: NaiveDateTime,
}

pub async fn create_ssh_key(
    pool: &PgPool,
    name: &str,
    public_key: &str,
    private_key: &str,
) -> Result<SshKey, sqlx::Error> {
    sqlx::query_as::<_, SshKey>(
        "INSERT INTO ssh_keys (name, public_key, private_key) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(name)
    .bind(public_key)
    .bind(private_key)
    .fetch_one(pool)
    .await
}

pub async fn list_ssh_keys(pool: &PgPool) -> Result<Vec<SshKey>, sqlx::Error> {
    sqlx::query_as::<_, SshKey>("SELECT * FROM ssh_keys ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn delete_ssh_key(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM ssh_keys WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The commit identity; empty strings when it has never been set.
pub async fn get_git_identity(pool: &PgPool) -> Result<crate::git::GitIdentity, sqlx::Error> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT author_name, author_email FROM git_settings")
            .fetch_optional(pool)
            .await?;
    Ok(row
        .map(|(name, email)| crate::git::GitIdentity { name, email })
        .unwrap_or_default())
}

pub async fn set_git_identity(
    pool: &PgPool,
    identity: &crate::git::GitIdentity,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO git_settings (author_name, author_email) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE
         SET author_name = $1, author_email = $2, updated_at = now()",
    )
    .bind(&identity.name)
    .bind(&identity.email)
    .execute(pool)
    .await?;
    Ok(())
}

// --- A conversation's git repos (SME-32) ---

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct ConversationRepo {
    pub id: i64,
    pub conversation_id: i64,
    pub url: String,
    pub remote_key: String,
    pub branch: Option<String>,
    pub dir: String,
    /// `cloning`, `ready` or `failed`.
    pub status: String,
    pub error: Option<String>,
    pub checked_out_branch: Option<String>,
    pub commit_sha: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    /// The checkout's AGENTS.md files, relative to it, top-level first.
    pub agents_files: Vec<String>,
    /// Which attempt at the clone is current; a retry increments it.
    pub attempt: i32,
}

/// Sets a checkout's AGENTS.md files, for tests; a clone records them
/// with `set_repo_ready`.
#[cfg(test)]
pub async fn set_repo_agents_files(pool: &PgPool, id: i64, files: &[String]) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversation_repos SET agents_files = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(files)
        .execute(pool)
        .await?;
    Ok(())
}

/// An AGENTS.md as read from a checkout, to load or to ask about.
#[derive(Clone, Debug, PartialEq)]
pub struct InstructionsFile {
    /// At most 32 KiB of it.
    pub content: String,
    pub file_bytes: i64,
    pub hash: String,
    pub commit: Option<String>,
}

/// An AGENTS.md in the model's context (see the migration).
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct LoadedInstruction {
    pub id: i64,
    pub conversation_id: i64,
    pub repo_id: i64,
    pub path: String,
    pub content: String,
    pub file_bytes: i64,
    pub hash: String,
    pub commit_sha: Option<String>,
    pub loaded_at: NaiveDateTime,
}

/// Loads `path` of repo `repo_id` into the conversation's context, or
/// replaces what was loaded from it.
pub async fn load_instruction(
    pool: &PgPool,
    conversation_id: i64,
    repo_id: i64,
    path: &str,
    file: &InstructionsFile,
) -> Result<LoadedInstruction, sqlx::Error> {
    sqlx::query_as::<_, LoadedInstruction>(
        "INSERT INTO loaded_instructions (conversation_id, repo_id, path, content, file_bytes, hash, commit_sha)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (repo_id, path) DO UPDATE
            SET content = $4, file_bytes = $5, hash = $6, commit_sha = $7, loaded_at = now()
         RETURNING *",
    )
    .bind(conversation_id)
    .bind(repo_id)
    .bind(path)
    .bind(&file.content)
    .bind(file.file_bytes)
    .bind(&file.hash)
    .bind(file.commit.as_deref())
    .fetch_one(pool)
    .await
}

/// The conversation's loaded AGENTS.md files, oldest first.
pub async fn list_loaded_instructions(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<LoadedInstruction>, sqlx::Error> {
    sqlx::query_as::<_, LoadedInstruction>(
        "SELECT * FROM loaded_instructions WHERE conversation_id = $1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// A load waiting on the user's trust decision (see the migration).
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct InstructionRequest {
    pub id: i64,
    pub conversation_id: i64,
    pub repo_id: i64,
    pub path: String,
    pub content: String,
    pub file_bytes: i64,
    pub hash: String,
    pub commit_sha: Option<String>,
    pub created_at: NaiveDateTime,
}

impl InstructionRequest {
    pub fn file(&self) -> InstructionsFile {
        InstructionsFile {
            content: self.content.clone(),
            file_bytes: self.file_bytes,
            hash: self.hash.clone(),
            commit: self.commit_sha.clone(),
        }
    }
}

/// Asks about `path` of repo `repo_id`, replacing an earlier request for
/// the same file (the card then shows the newest copy).
pub async fn request_instruction(
    pool: &PgPool,
    conversation_id: i64,
    repo_id: i64,
    path: &str,
    file: &InstructionsFile,
) -> Result<InstructionRequest, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>(
        "INSERT INTO instruction_requests (conversation_id, repo_id, path, content, file_bytes, hash, commit_sha)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (repo_id, path) DO UPDATE
            SET content = $4, file_bytes = $5, hash = $6, commit_sha = $7, created_at = now()
         RETURNING *",
    )
    .bind(conversation_id)
    .bind(repo_id)
    .bind(path)
    .bind(&file.content)
    .bind(file.file_bytes)
    .bind(&file.hash)
    .bind(file.commit.as_deref())
    .fetch_one(pool)
    .await
}

pub async fn get_instruction_request(pool: &PgPool, id: i64) -> Result<Option<InstructionRequest>, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>("SELECT * FROM instruction_requests WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_instruction_requests(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<InstructionRequest>, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>(
        "SELECT * FROM instruction_requests WHERE conversation_id = $1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// Every conversation's requests about repos with this remote.
pub async fn list_instruction_requests_for_remote(
    pool: &PgPool,
    remote_key: &str,
) -> Result<Vec<InstructionRequest>, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>(
        "SELECT r.* FROM instruction_requests r
           JOIN conversation_repos c ON c.id = r.repo_id
          WHERE c.remote_key = $1 ORDER BY r.id ASC",
    )
    .bind(remote_key)
    .fetch_all(pool)
    .await
}

/// Unloads every conversation's instructions from repos with this remote
/// (the user declined it). Returns `(conversation, checkout dir, path)` of
/// each file unloaded.
pub async fn unload_instructions_for_remote(
    pool: &PgPool,
    remote_key: &str,
) -> Result<Vec<(i64, String, String)>, sqlx::Error> {
    sqlx::query_as(
        "DELETE FROM loaded_instructions l
          USING conversation_repos c
          WHERE c.id = l.repo_id AND c.remote_key = $1
      RETURNING l.conversation_id, c.dir, l.path",
    )
    .bind(remote_key)
    .fetch_all(pool)
    .await
}

/// Drops a pending request for `path` of repo `repo_id`, if there is one.
pub async fn delete_instruction_request_for(pool: &PgPool, repo_id: i64, path: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM instruction_requests WHERE repo_id = $1 AND path = $2")
        .bind(repo_id)
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_instruction_request(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM instruction_requests WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_conversation_repo(pool: &PgPool, id: i64) -> Result<Option<ConversationRepo>, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>("SELECT * FROM conversation_repos WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct RepoTrust {
    pub remote_key: String,
    pub trusted: bool,
    pub decided_at: NaiveDateTime,
}

/// `Some(trusted)` once the user has decided about this remote.
pub async fn get_repo_trust(pool: &PgPool, remote_key: &str) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar("SELECT trusted FROM repo_trust WHERE remote_key = $1")
        .bind(remote_key)
        .fetch_optional(pool)
        .await
}

pub async fn set_repo_trust(pool: &PgPool, remote_key: &str, trusted: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO repo_trust (remote_key, trusted) VALUES ($1, $2)
         ON CONFLICT (remote_key) DO UPDATE SET trusted = $2, decided_at = now()",
    )
    .bind(remote_key)
    .bind(trusted)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_repo_trust(pool: &PgPool) -> Result<Vec<RepoTrust>, sqlx::Error> {
    sqlx::query_as::<_, RepoTrust>("SELECT * FROM repo_trust ORDER BY remote_key ASC")
        .fetch_all(pool)
        .await
}

/// Forgets a decision, so the user is asked again next time.
pub async fn delete_repo_trust(pool: &PgPool, remote_key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM repo_trust WHERE remote_key = $1")
        .bind(remote_key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records a repo, `cloning`. Fails on a `dir` the conversation already uses.
pub async fn create_conversation_repo(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    remote_key: &str,
    branch: Option<&str>,
    dir: &str,
) -> Result<ConversationRepo, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>(
        "INSERT INTO conversation_repos (conversation_id, url, remote_key, branch, dir, status)
         VALUES ($1, $2, $3, $4, $5, 'cloning') RETURNING *",
    )
    .bind(conversation_id)
    .bind(url)
    .bind(remote_key)
    .bind(branch)
    .bind(dir)
    .fetch_one(pool)
    .await
}

/// A conversation's repos, in the order they were added.
pub async fn list_conversation_repos(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<ConversationRepo>, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>(
        "SELECT * FROM conversation_repos WHERE conversation_id = $1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// A failed clone is being retried, with the URL and branch asked for
/// this time (the same remote, maybe written another way). `None` when the
/// clone isn't failed any more (another retry got there first).
pub async fn retry_repo_clone(
    pool: &PgPool,
    id: i64,
    url: &str,
    remote_key: &str,
    branch: Option<&str>,
) -> Result<Option<ConversationRepo>, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>(
        "UPDATE conversation_repos
            SET url = $2, remote_key = $3, branch = $4, status = 'cloning', error = NULL,
                agents_files = '{}', attempt = attempt + 1, updated_at = now()
          WHERE id = $1 AND status = 'failed'
      RETURNING *",
    )
    .bind(id)
    .bind(url)
    .bind(remote_key)
    .bind(branch)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
pub async fn set_repo_cloned(
    pool: &PgPool,
    id: i64,
    checked_out_branch: &str,
    commit_sha: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE conversation_repos
            SET status = 'ready', error = NULL, checked_out_branch = $2, commit_sha = $3,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(checked_out_branch)
    .bind(commit_sha)
    .execute(pool)
    .await?;
    Ok(())
}

/// A clone that finished, with the AGENTS.md files it has (or why they
/// couldn't be listed) recorded in the same update, so nothing sees it
/// ready without them. Only `attempt`, still cloning, can finish it
/// (SME-86); false when it's no longer the current attempt.
pub async fn set_repo_ready(
    pool: &PgPool,
    id: i64,
    attempt: i32,
    checked_out_branch: &str,
    commit_sha: Option<&str>,
    agents_files: &[String],
    error: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos
            SET status = 'ready', error = $4, checked_out_branch = $2, commit_sha = $3,
                agents_files = $5, updated_at = now()
          WHERE id = $1 AND status = 'cloning' AND attempt = $6",
    )
    .bind(id)
    .bind(checked_out_branch)
    .bind(commit_sha)
    .bind(error)
    .bind(agents_files)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// At startup: a clone still marked cloning was cut off by the restart.
/// Returns how many there were.
pub async fn fail_unfinished_clones(pool: &PgPool, error: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos SET status = 'failed', error = $1, updated_at = now()
          WHERE status = 'cloning'",
    )
    .bind(error)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Attempt `attempt` at a clone failed. Like `set_repo_ready`, only the
/// current attempt, still cloning, can; false otherwise.
pub async fn set_repo_failed(pool: &PgPool, id: i64, attempt: i32, error: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos SET status = 'failed', error = $2, updated_at = now()
          WHERE id = $1 AND status = 'cloning' AND attempt = $3",
    )
    .bind(id)
    .bind(error)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

// --- MCP servers (externally-hosted, configured via the /mcp-servers UI) ---
// Plain CRUD, no soft delete — this is configuration a person edits, not a
// live external resource like a sandbox pod. See
// SME-15.

/// Holds header values and OAuth secrets, so it has no `Serialize` and its
/// `Debug` hides them (SME-60): the UI gets `api::mcp::McpServerSummary`.
#[derive(Clone, PartialEq, sqlx::FromRow)]
pub struct McpServerConfig {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub extra_headers: sqlx::types::Json<HashMap<String, String>>,
    /// `"static_headers"` or `"oauth"` — see
    /// SME-16. A plain `String`, not a Rust enum:
    /// the DB-level `CHECK` constraint is the source of truth for valid
    /// values, matching `terminal_commands.status`'s existing precedent
    /// elsewhere in this file.
    pub auth_mode: String,
    /// `rmcp::transport::auth::StoredCredentials`, serialized as-is —
    /// `NULL` means "oauth mode, never connected (or disconnected)".
    /// Untyped `serde_json::Value` here (not the `StoredCredentials` type
    /// itself) since this module doesn't otherwise depend on `rmcp`;
    /// `PgCredentialStore` (`src/mcp_oauth.rs`) does the typed
    /// (de)serialization at its load/save boundary.
    pub oauth_credentials: Option<sqlx::types::Json<serde_json::Value>>,
    /// A pre-registered OAuth client, for a provider that publishes no
    /// discovery metadata and rejects Dynamic Client Registration — GitHub,
    /// confirmed live (see SME-16). Set only at
    /// creation time (`McpServerNew`), same "delete and recreate to
    /// change" precedent as `auth_mode` itself. `oauth_client_id` isn't
    /// secret — it's visible in the authorization URL sent to the browser
    /// regardless — so it's shown back plain, unlike `oauth_client_secret`
    /// (write-only, `extra_headers`' precedent).
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl std::fmt::Debug for McpServerConfig {
    /// Header names, the client id and whether credentials are stored, but
    /// no header value, credential or client secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut header_names: Vec<&String> = self.extra_headers.0.keys().collect();
        header_names.sort();
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("url", &self.url)
            .field("extra_headers", &header_names)
            .field("auth_mode", &self.auth_mode)
            .field("oauth_credentials", &self.oauth_credentials.as_ref().map(|_| "<stored>"))
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_client_secret", &self.oauth_client_secret.as_ref().map(|_| "<set>"))
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

#[cfg(test)]
mod mcp_server_config_debug_tests {
    use super::*;

    #[test]
    fn test_an_mcp_server_configs_debug_output_hides_its_secrets() {
        let config = McpServerConfig {
            id: 7,
            name: "github".to_string(),
            url: "https://api.githubcopilot.com/mcp/".to_string(),
            extra_headers: sqlx::types::Json(HashMap::from([(
                "Authorization".to_string(),
                "Bearer header-secret-123".to_string(),
            )])),
            auth_mode: "oauth".to_string(),
            oauth_credentials: Some(sqlx::types::Json(serde_json::json!({"access_token": "token-secret-456"}))),
            oauth_client_id: Some("Iv1.public-client-id".to_string()),
            oauth_client_secret: Some("client-secret-789".to_string()),
            created_at: NaiveDateTime::default(),
            updated_at: NaiveDateTime::default(),
        };
        let shown = format!("{config:?}");
        for secret in ["header-secret-123", "token-secret-456", "client-secret-789"] {
            assert!(!shown.contains(secret), "Debug output shows {secret}: {shown}");
        }
        for kept in ["github", "Authorization", "Iv1.public-client-id", "oauth"] {
            assert!(shown.contains(kept), "Debug output should still show {kept}: {shown}");
        }
    }
}

pub async fn create_mcp_server_config(
    pool: &PgPool,
    name: &str,
    url: &str,
    extra_headers: &HashMap<String, String>,
    auth_mode: &str,
    oauth_client_id: Option<&str>,
    oauth_client_secret: Option<&str>,
) -> Result<McpServerConfig, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>(
        "INSERT INTO mcp_servers (name, url, extra_headers, auth_mode, oauth_client_id, oauth_client_secret)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(name)
    .bind(url)
    .bind(sqlx::types::Json(extra_headers))
    .bind(auth_mode)
    .bind(oauth_client_id)
    .bind(oauth_client_secret)
    .fetch_one(pool)
    .await
}

pub async fn list_mcp_server_configs(pool: &PgPool) -> Result<Vec<McpServerConfig>, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>("SELECT * FROM mcp_servers ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn get_mcp_server_config(
    pool: &PgPool,
    id: i64,
) -> Result<Option<McpServerConfig>, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>("SELECT * FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Resolves a `mcp__<server_name>__<tool_name>` tool call's server half —
/// `name` is unique, so this is how `anthropic::tools::execute`'s MCP
/// dispatch arm turns the model-facing name back into a row.
pub async fn get_mcp_server_config_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<McpServerConfig>, sqlx::Error> {
    sqlx::query_as::<_, McpServerConfig>("SELECT * FROM mcp_servers WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
}

/// Updates a server's name, URL, and headers in one shot — the single save
/// action the `/mcp-servers/{id}` edit page's one form drives. Name/URL are
/// set outright; headers are *merged*: `upsert` adds/overwrites just those
/// names and `remove` drops just those names, so every other existing
/// header (including ones the browser never received a value for — see
/// `McpServerSummary`) is left exactly as it was. If a name appears in both
/// `upsert` and `remove`, `upsert` wins — the header ends up set, not
/// deleted (arbitrary but deterministic; the UI never actually produces
/// this case, since a header row is either edited or removed, never both).
/// A URL change clears any stored `oauth_credentials` — see
/// SME-16's "Data model": a grant is bound to the
/// audience it was issued for, so carrying it across a URL change would
/// silently point a stale token at a different resource.
pub async fn update_mcp_server_config(
    pool: &PgPool,
    id: i64,
    name: &str,
    url: &str,
    upsert: &HashMap<String, String>,
    remove: &[String],
    auth_mode: &str,
) -> Result<Option<McpServerConfig>, sqlx::Error> {
    let Some(existing) = get_mcp_server_config(pool, id).await? else {
        return Ok(None);
    };

    let mut merged = existing.extra_headers.0;
    for name in remove {
        merged.remove(name);
    }
    merged.extend(upsert.iter().map(|(k, v)| (k.clone(), v.clone())));

    let url_changed = existing.url != url;

    sqlx::query_as::<_, McpServerConfig>(
        "UPDATE mcp_servers SET name = $2, url = $3, extra_headers = $4, auth_mode = $5,
             oauth_credentials = CASE WHEN $6 THEN NULL ELSE oauth_credentials END,
             updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(url)
    .bind(sqlx::types::Json(merged))
    .bind(auth_mode)
    .bind(url_changed)
    .fetch_optional(pool)
    .await
}

/// A `language_servers` row (SME-35); its list and map columns are JSON.
#[derive(Debug, sqlx::FromRow)]
struct LanguageServerRow {
    id: i64,
    name: String,
    image: String,
    install_command: String,
    command: String,
    args: sqlx::types::Json<Vec<String>>,
    env: sqlx::types::Json<std::collections::BTreeMap<String, String>>,
    file_types: sqlx::types::Json<std::collections::BTreeMap<String, String>>,
    root_markers: sqlx::types::Json<Vec<String>>,
    initialization_options: Option<serde_json::Value>,
    settings: Option<serde_json::Value>,
    memory_limit: String,
    cpu_limit: String,
    enabled: bool,
    updated_at: NaiveDateTime,
}

impl From<LanguageServerRow> for crate::models::LanguageServer {
    fn from(row: LanguageServerRow) -> Self {
        crate::models::LanguageServer {
            id: row.id,
            config: crate::models::LanguageServerConfig {
                name: row.name,
                image: row.image,
                install_command: row.install_command,
                command: row.command,
                args: row.args.0,
                env: row.env.0,
                file_types: row.file_types.0,
                root_markers: row.root_markers.0,
                initialization_options: row.initialization_options,
                settings: row.settings,
                memory_limit: row.memory_limit,
                cpu_limit: row.cpu_limit,
                enabled: row.enabled,
            },
            updated_at: row.updated_at,
        }
    }
}

const LANGUAGE_SERVER_COLUMNS: &str = "id, name, image, install_command, command, args, env, file_types, \
     root_markers, initialization_options, settings, memory_limit, cpu_limit, enabled, updated_at";

pub async fn create_language_server(
    pool: &PgPool,
    config: &crate::models::LanguageServerConfig,
) -> Result<crate::models::LanguageServer, sqlx::Error> {
    let sql = format!(
        "INSERT INTO language_servers (name, image, install_command, command, args, env, file_types, \
             root_markers, initialization_options, settings, memory_limit, cpu_limit, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING {LANGUAGE_SERVER_COLUMNS}"
    );
    bind_language_server(sqlx::query_as::<_, LanguageServerRow>(&sql), config)
        .fetch_one(pool)
        .await
        .map(Into::into)
}

/// Replaces `id`'s settings; `None` if there's no such server.
pub async fn update_language_server(
    pool: &PgPool,
    id: i64,
    config: &crate::models::LanguageServerConfig,
) -> Result<Option<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!(
        "UPDATE language_servers SET name = $1, image = $2, install_command = $3, command = $4, args = $5, \
             env = $6, file_types = $7, root_markers = $8, initialization_options = $9, settings = $10, \
             memory_limit = $11, cpu_limit = $12, enabled = $13, updated_at = now() \
         WHERE id = $14 RETURNING {LANGUAGE_SERVER_COLUMNS}"
    );
    bind_language_server(sqlx::query_as::<_, LanguageServerRow>(&sql), config)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|row| row.map(Into::into))
}

fn bind_language_server<'q>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, LanguageServerRow, sqlx::postgres::PgArguments>,
    config: &'q crate::models::LanguageServerConfig,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, LanguageServerRow, sqlx::postgres::PgArguments> {
    query
        .bind(&config.name)
        .bind(&config.image)
        .bind(&config.install_command)
        .bind(&config.command)
        .bind(sqlx::types::Json(&config.args))
        .bind(sqlx::types::Json(&config.env))
        .bind(sqlx::types::Json(&config.file_types))
        .bind(sqlx::types::Json(&config.root_markers))
        .bind(&config.initialization_options)
        .bind(&config.settings)
        .bind(&config.memory_limit)
        .bind(&config.cpu_limit)
        .bind(config.enabled)
}

pub async fn list_language_servers(pool: &PgPool) -> Result<Vec<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!("SELECT {LANGUAGE_SERVER_COLUMNS} FROM language_servers ORDER BY name");
    sqlx::query_as::<_, LanguageServerRow>(&sql)
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().map(Into::into).collect())
}

pub async fn get_language_server(pool: &PgPool, id: i64) -> Result<Option<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!("SELECT {LANGUAGE_SERVER_COLUMNS} FROM language_servers WHERE id = $1");
    sqlx::query_as::<_, LanguageServerRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|row| row.map(Into::into))
}

/// Deletes `id`, returning what it was (for stopping its pods), or `None`.
pub async fn delete_language_server(
    pool: &PgPool,
    id: i64,
) -> Result<Option<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!("DELETE FROM language_servers WHERE id = $1 RETURNING {LANGUAGE_SERVER_COLUMNS}");
    sqlx::query_as::<_, LanguageServerRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|row| row.map(Into::into))
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

/// Adds an MCP server named `name` unless one with that name already
/// exists, for the servers smelt ships with (`mcp::default_mcp_servers`).
/// Matched by name only, so a user's edits to the entry (a key header, a
/// new URL, OAuth) are kept. Returns whether it inserted.
pub async fn ensure_mcp_server(pool: &PgPool, name: &str, url: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO mcp_servers (name, url) VALUES ($1, $2)
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(name)
    .bind(url)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn delete_mcp_server_config(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets or clears a server's stored OAuth credentials — the persistence
/// half of `mcp_oauth::PgCredentialStore`'s `save`/`clear`. `None` clears
/// (the same as a fresh `oauth` server that has never connected).
pub async fn set_mcp_server_oauth_credentials(
    pool: &PgPool,
    id: i64,
    credentials: Option<serde_json::Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE mcp_servers SET oauth_credentials = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(credentials.map(sqlx::types::Json))
        .execute(pool)
        .await?;
    Ok(())
}

/// Saves a refreshed grant only while the row still holds a grant for the
/// same client: a refresh that finishes after a Disconnect, a URL change
/// or a new Connect mustn't write back over what they wrote (SME-113).
/// `false` when nothing matched.
pub async fn save_refreshed_mcp_server_oauth_credentials(
    pool: &PgPool,
    id: i64,
    client_id: &str,
    credentials: serde_json::Value,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE mcp_servers SET oauth_credentials = $3, updated_at = now() \
         WHERE id = $1 AND oauth_credentials->>'client_id' = $2",
    )
    .bind(id)
    .bind(client_id)
    .bind(sqlx::types::Json(credentials))
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

// --- Model providers (SME-72) ---
// Configuration a person edits on /providers, like mcp_servers: plain
// CRUD. `kind` and `auth_kind` are plain strings, the table's `CHECK`s
// being the source of truth (`McpServerConfig::auth_mode`'s precedent);
// `providers.rs` turns them into enums at its boundary.

#[derive(Clone, PartialEq, sqlx::FromRow)]
pub struct InferenceProvider {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub auth_kind: String,
    /// Never sent to the browser; `providers::secret_hint` is.
    pub secret: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// By hand, so the secret never reaches a log line.
impl std::fmt::Debug for InferenceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceProvider")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("auth_kind", &self.auth_kind)
            .field("secret", &"..")
            .finish_non_exhaustive()
    }
}

pub async fn create_inference_provider(
    pool: &PgPool,
    name: &str,
    kind: &str,
    base_url: &str,
    auth_kind: &str,
    secret: &str,
) -> Result<InferenceProvider, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>(
        "INSERT INTO inference_providers (name, kind, base_url, auth_kind, secret)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(name)
    .bind(kind)
    .bind(base_url)
    .bind(auth_kind)
    .bind(secret)
    .fetch_one(pool)
    .await
}

pub async fn list_inference_providers(pool: &PgPool) -> Result<Vec<InferenceProvider>, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>("SELECT * FROM inference_providers ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn get_inference_provider(
    pool: &PgPool,
    id: i64,
) -> Result<Option<InferenceProvider>, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>("SELECT * FROM inference_providers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Sets every field outright, except `secret`: `None` keeps the stored
/// one, so the edit form never needs to know it.
pub async fn update_inference_provider(
    pool: &PgPool,
    id: i64,
    name: &str,
    kind: &str,
    base_url: &str,
    auth_kind: &str,
    secret: Option<&str>,
) -> Result<Option<InferenceProvider>, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>(
        "UPDATE inference_providers
         SET name = $2, kind = $3, base_url = $4, auth_kind = $5,
             secret = COALESCE($6, secret), updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(kind)
    .bind(base_url)
    .bind(auth_kind)
    .bind(secret)
    .fetch_optional(pool)
    .await
}

/// Deletes a provider, first clearing it as the default and from every
/// conversation using it, all in one transaction: those conversations take
/// the default at their next turn. The foreign keys are `RESTRICT`, so a
/// reference this missed fails the delete instead of leaving a model with
/// no provider. Returns whether the provider existed.
pub async fn delete_inference_provider(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE inference_settings SET default_provider_id = NULL, default_model = NULL
         WHERE default_provider_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET provider_id = NULL, model = NULL WHERE provider_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let deleted = sqlx::query("DELETE FROM inference_providers WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(deleted == 1)
}

/// A model's row in `provider_models`: the user's overrides and what the
/// provider last reported. Null means unset or not reported.
#[derive(Clone, Debug, Default, PartialEq, sqlx::FromRow)]
pub struct ProviderModelRow {
    pub provider_id: i64,
    pub model: String,
    pub thinking: Option<bool>,
    pub context_window: Option<i32>,
    pub reported_context_window: Option<i32>,
    pub reported_thinking: Option<bool>,
    pub reported_tools: Option<bool>,
    /// Added on the provider's page as a model its listing doesn't show,
    /// so it's shown even when the listing works and lacks it.
    pub added_by_hand: bool,
}

pub async fn get_provider_model(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
) -> Result<Option<ProviderModelRow>, sqlx::Error> {
    sqlx::query_as::<_, ProviderModelRow>(
        "SELECT * FROM provider_models WHERE provider_id = $1 AND model = $2",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_optional(pool)
    .await
}

pub async fn list_provider_models(
    pool: &PgPool,
    provider_id: i64,
) -> Result<Vec<ProviderModelRow>, sqlx::Error> {
    sqlx::query_as::<_, ProviderModelRow>(
        "SELECT * FROM provider_models WHERE provider_id = $1 ORDER BY model ASC",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await
}

/// Marks a model as added by hand, adding its row if it has none and
/// leaving its settings as they are.
pub async fn ensure_provider_model(pool: &PgPool, provider_id: i64, model: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models (provider_id, model, added_by_hand) VALUES ($1, $2, true)
         ON CONFLICT (provider_id, model) DO UPDATE SET added_by_hand = true",
    )
    .bind(provider_id)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sets the user's overrides for a model, leaving what the provider
/// reported alone. `None` clears an override.
pub async fn set_provider_model_overrides(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
    thinking: Option<bool>,
    context_window: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models (provider_id, model, thinking, context_window)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider_id, model) DO UPDATE
         SET thinking = $3, context_window = $4",
    )
    .bind(provider_id)
    .bind(model)
    .bind(thinking)
    .bind(context_window)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces what the provider reported about a model, leaving the user's
/// overrides alone. A report without a context window keeps the last one:
/// Ollama only says while the model is loaded.
pub async fn set_provider_model_reported(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
    context_window: Option<i32>,
    thinking: Option<bool>,
    tools: Option<bool>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models
             (provider_id, model, reported_context_window, reported_thinking, reported_tools)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (provider_id, model) DO UPDATE
         SET reported_context_window = COALESCE($3, provider_models.reported_context_window),
             reported_thinking = $4, reported_tools = $5",
    )
    .bind(provider_id)
    .bind(model)
    .bind(context_window)
    .bind(thinking)
    .bind(tools)
    .execute(pool)
    .await?;
    Ok(())
}

/// The default provider and model, if one is set.
pub async fn get_default_model(pool: &PgPool) -> Result<Option<(i64, String)>, sqlx::Error> {
    let row: Option<(Option<i64>, Option<String>)> =
        sqlx::query_as("SELECT default_provider_id, default_model FROM inference_settings")
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        Some((Some(provider_id), Some(model))) => Some((provider_id, model)),
        _ => None,
    })
}

pub async fn set_default_model(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO inference_settings (default_provider_id, default_model) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE SET default_provider_id = $1, default_model = $2",
    )
    .bind(provider_id)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// A conversation's own provider and model (both `None` until its first
/// turn takes the default), and its last message when they last changed.
#[derive(Clone, Debug, Default, PartialEq, sqlx::FromRow)]
pub struct ConversationModelRow {
    pub provider_id: Option<i64>,
    pub model: Option<String>,
    pub model_changed_at_message_id: Option<i64>,
}

/// `None` when the conversation doesn't exist.
pub async fn get_conversation_model(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<ConversationModelRow>, sqlx::Error> {
    sqlx::query_as::<_, ConversationModelRow>(
        "SELECT provider_id, model, model_changed_at_message_id FROM conversations WHERE id = $1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
}

/// Sets a conversation's provider and model, from its next turn on. The
/// switch point for its thinking blocks is recorded when that turn starts
/// (`record_turn_model`), since a turn still running keeps writing for the
/// old model. Returns whether the conversation exists.
pub async fn set_conversation_model(
    pool: &PgPool,
    conversation_id: i64,
    provider_id: i64,
    model: &str,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query("UPDATE conversations SET provider_id = $2, model = $3 WHERE id = $1")
    .bind(conversation_id)
    .bind(provider_id)
    .bind(model)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// Records that a turn is starting on the backend `model_key` names
/// (provider, base URL and model), and returns the message up to which
/// thinking must be left out: when it differs from the last turn's
/// backend, that's the conversation's last message now. Call under the
/// conversation's turn lock, so no turn is writing meanwhile.
pub async fn record_turn_model(
    pool: &PgPool,
    conversation_id: i64,
    model_key: &str,
) -> Result<Option<i64>, sqlx::Error> {
    // The right-hand sides read the row's values from before the update.
    let stamp: Option<(Option<i64>,)> = sqlx::query_as(
        "UPDATE conversations
         SET model_changed_at_message_id = CASE
                 WHEN last_turn_model_key IS DISTINCT FROM $2
                 THEN (SELECT max(id) FROM messages WHERE conversation_id = $1)
                 ELSE model_changed_at_message_id
             END,
             last_turn_model_key = $2
         WHERE id = $1
         RETURNING model_changed_at_message_id",
    )
    .bind(conversation_id)
    .bind(model_key)
    .fetch_optional(pool)
    .await?;
    Ok(stamp.and_then(|(id,)| id))
}

/// Gives a conversation with no model the default, in one statement so a
/// racing change isn't overwritten. Returns the provider and model it took,
/// or `None` when it already had one or there's no default.
pub async fn adopt_default_model(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    let adopted: Option<(i64, String)> = sqlx::query_as(
        "UPDATE conversations c
         SET provider_id = s.default_provider_id, model = s.default_model
         FROM inference_settings s
         WHERE c.id = $1 AND c.provider_id IS NULL AND s.default_provider_id IS NOT NULL
         RETURNING c.provider_id, c.model",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SME-35: a language server's settings go in and come back whole,
    /// and a second server can't take a name that's in use.
    #[sqlx::test]
    async fn test_language_servers_round_trip(pool: PgPool) {
        let mut config = crate::models::LanguageServerConfig {
            name: "pyright".to_string(),
            image: "node:22-slim".to_string(),
            install_command: "npm install --prefix $HOME/lsp pyright".to_string(),
            command: "pyright-langserver".to_string(),
            args: vec!["--stdio".to_string()],
            env: [("NODE_OPTIONS".to_string(), "--max-old-space-size=1024".to_string())].into(),
            file_types: [("py".to_string(), "python".to_string()), ("pyi".to_string(), "python".to_string())].into(),
            root_markers: vec!["pyproject.toml".to_string()],
            initialization_options: None,
            settings: Some(serde_json::json!({"python": {"analysis": {"typeCheckingMode": "basic"}}})),
            memory_limit: "1Gi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
        };
        let created = create_language_server(&pool, &config).await.expect("create");
        assert_eq!(created.config, config);
        assert_eq!(get_language_server(&pool, created.id).await.expect("get"), Some(created.clone()));

        let duplicate = create_language_server(&pool, &config).await;
        assert!(duplicate.as_ref().is_err_and(|e| e.as_database_error().is_some_and(|d| d.is_unique_violation())));

        config.enabled = false;
        config.args.push("--verbose".to_string());
        let updated = update_language_server(&pool, created.id, &config).await.expect("update").expect("exists");
        assert_eq!(updated.config, config);
        assert_eq!(list_language_servers(&pool).await.expect("list"), vec![updated.clone()]);

        assert_eq!(delete_language_server(&pool, created.id).await.expect("delete").map(|s| s.id), Some(created.id));
        assert!(list_language_servers(&pool).await.expect("list").is_empty());
        assert_eq!(update_language_server(&pool, created.id, &config).await.expect("update"), None);
    }

    /// SME-51 B7: one live pod per conversation is the database's rule,
    /// not only a check `create_pod` makes before inserting.
    #[sqlx::test]
    async fn test_a_conversation_cant_have_two_live_pods(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("conversation");
        let first = create_sandbox_pod(&pool, conversation.id).await.expect("first pod");
        let second = create_sandbox_pod(&pool, conversation.id).await;
        assert!(
            second.as_ref().is_err_and(|e| e.as_database_error().is_some_and(|d| d.is_unique_violation())),
            "a second live pod was recorded: {second:?}"
        );
        terminate_sandbox_pod(&pool, first.id).await.expect("terminate");
        create_sandbox_pod(&pool, conversation.id).await.expect("a pod after the first ended");
    }

    /// SME-41 D11: the default title is now sentence case; a conversation
    /// still carrying the old "New Conversation" gets auto-titled too.
    #[sqlx::test]
    async fn test_an_old_style_default_title_is_still_replaced(pool: PgPool) {
        assert_eq!(DEFAULT_TITLE, "New conversation");
        let conversation = create_conversation(&pool).await.expect("create conversation");
        sqlx::query("UPDATE conversations SET title = 'New Conversation' WHERE id = $1")
            .bind(conversation.id)
            .execute(&pool)
            .await
            .expect("give it the old title");
        create_message(
            &pool,
            conversation.id,
            "user",
            &[crate::anthropic::ContentBlock::Text { text: "fix the build".to_string() }],
        )
        .await
        .expect("send a message");
        let titled = list_conversations(&pool).await.expect("list").into_iter().find(|c| c.id == conversation.id).expect("found");
        assert_eq!(titled.title, "fix the build");
    }

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
    async fn test_get_conversation_usage_returns_none_when_never_set(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        assert_eq!(
            get_conversation_usage(&pool, conversation.id)
                .await
                .expect("lookup"),
            None
        );
    }

    #[sqlx::test]
    async fn test_upsert_conversation_usage_round_trips_and_overwrites(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        let first = crate::anthropic::TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 0,
        };
        upsert_conversation_usage(&pool, conversation.id, &first)
            .await
            .expect("upsert should succeed");
        assert_eq!(
            get_conversation_usage(&pool, conversation.id)
                .await
                .expect("lookup"),
            Some(first)
        );

        // A second call overwrites, rather than accumulating — "last known
        // only," not a history.
        let second = crate::anthropic::TokenUsage {
            input_tokens: 150,
            output_tokens: 30,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 5,
        };
        upsert_conversation_usage(&pool, conversation.id, &second)
            .await
            .expect("second upsert should succeed");
        assert_eq!(
            get_conversation_usage(&pool, conversation.id)
                .await
                .expect("lookup"),
            Some(second)
        );
    }

    fn sample_questions() -> Vec<crate::questions::Question> {
        crate::questions::parse_questions(&serde_json::json!({"questions": [
            {"question": "Delete it?", "header": "Delete", "options": [{"label": "Yes"}, {"label": "No"}]}
        ]}))
        .expect("valid questions")
    }

    fn sample_answer(label: &str) -> Vec<crate::questions::QuestionAnswer> {
        vec![crate::questions::QuestionAnswer { selected: vec![label.to_string()], other: None }]
    }

    #[sqlx::test]
    async fn test_a_pending_question_is_answered_once_then_taken(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("conversation");
        let id = conversation.id;
        assert_eq!(get_pending_question(&pool, id).await.expect("get"), None);
        create_pending_question(&pool, id, "toolu_q", &sample_questions()).await.expect("create");
        let waiting = get_pending_question(&pool, id).await.expect("get").expect("a question");
        assert_eq!(waiting.tool_use_id, "toolu_q");
        assert_eq!(waiting.questions, sample_questions());
        assert_eq!(waiting.answer, None);
        assert_eq!(list_waiting_conversations(&pool).await.expect("list"), vec![id]);

        // A waiting question stays unless the take includes waiting ones.
        assert_eq!(take_pending_question(&pool, id, false).await.expect("take"), None);
        assert!(!answer_pending_question(&pool, id, "toolu_other", &sample_answer("Yes")).await.expect("answer"));
        assert!(answer_pending_question(&pool, id, "toolu_q", &sample_answer("Yes")).await.expect("answer"));
        assert!(
            !answer_pending_question(&pool, id, "toolu_q", &sample_answer("No")).await.expect("answer"),
            "a second answer must not replace the first"
        );
        assert!(list_waiting_conversations(&pool).await.expect("list").is_empty());

        let taken = take_pending_question(&pool, id, false).await.expect("take").expect("the answered question");
        assert_eq!(taken.answer, Some(sample_answer("Yes")));
        assert_eq!(get_pending_question(&pool, id).await.expect("get"), None);
    }

    #[sqlx::test]
    async fn test_a_waiting_question_is_taken_only_when_asked_for(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("conversation");
        create_pending_question(&pool, conversation.id, "toolu_q", &sample_questions()).await.expect("create");
        let taken = take_pending_question(&pool, conversation.id, true).await.expect("take").expect("the waiting question");
        assert_eq!(taken.answer, None);
        assert_eq!(get_pending_question(&pool, conversation.id).await.expect("get"), None);
    }

    #[sqlx::test]
    async fn test_a_pending_question_goes_with_its_conversation(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("conversation");
        create_pending_question(&pool, conversation.id, "toolu_q", &sample_questions()).await.expect("create");
        delete_conversation(&pool, conversation.id).await.expect("delete");
        assert!(list_waiting_conversations(&pool).await.expect("list").is_empty());
    }

    #[sqlx::test]
    async fn test_get_conversation_todos_is_empty_when_never_written(pool: PgPool) {
        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        assert_eq!(
            get_conversation_todos(&pool, conversation.id)
                .await
                .expect("lookup"),
            Vec::new()
        );
    }

    #[sqlx::test]
    async fn test_set_conversation_todos_round_trips_and_overwrites(pool: PgPool) {
        use crate::anthropic::tools::{TodoItem, TodoStatus};

        let conversation = create_conversation(&pool)
            .await
            .expect("create conversation");
        let first = vec![
            TodoItem {
                content: "write the plan".to_string(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "implement".to_string(),
                status: TodoStatus::InProgress,
            },
        ];
        set_conversation_todos(&pool, conversation.id, &first)
            .await
            .expect("set should succeed");
        assert_eq!(
            get_conversation_todos(&pool, conversation.id)
                .await
                .expect("lookup"),
            first
        );

        // A second call overwrites wholesale, rather than merging with the
        // first — "last known only," not an append log.
        let second = vec![TodoItem {
            content: "ship it".to_string(),
            status: TodoStatus::Pending,
        }];
        set_conversation_todos(&pool, conversation.id, &second)
            .await
            .expect("second set should succeed");
        assert_eq!(
            get_conversation_todos(&pool, conversation.id)
                .await
                .expect("lookup"),
            second
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

    // --- Terminal ---

    async fn test_conversation(pool: &PgPool) -> Conversation {
        create_conversation(pool)
            .await
            .expect("create conversation")
    }

    #[sqlx::test]
    async fn test_list_live_pods_reports_each_live_pod_and_its_activity(pool: PgPool) {
        let busy = test_conversation(&pool).await;
        create_message(
            &pool,
            busy.id,
            "user",
            &[ContentBlock::Text { text: "build it".to_string() }],
        )
        .await
        .expect("title the conversation");
        let busy_pod = create_sandbox_pod(&pool, busy.id).await.expect("pod");
        let t1 = create_sandbox_terminal(&pool, busy_pod.id).await.expect("terminal");
        let t2 = create_sandbox_terminal(&pool, busy_pod.id).await.expect("terminal");
        create_terminal_command(&pool, busy.id, t1.id, "cmd-done", "true")
            .await
            .expect("command");
        mark_terminal_command_finished(&pool, "cmd-done", 0).await.expect("finish");
        create_terminal_command(&pool, busy.id, t2.id, "cmd-running", "sleep 100")
            .await
            .expect("command");
        let closed = create_sandbox_terminal(&pool, busy_pod.id).await.expect("terminal");
        terminate_sandbox_terminal(&pool, closed.id).await.expect("close terminal");

        let quiet = test_conversation(&pool).await;
        let quiet_pod = create_sandbox_pod(&pool, quiet.id).await.expect("pod");

        let gone = test_conversation(&pool).await;
        let gone_pod = create_sandbox_pod(&pool, gone.id).await.expect("pod");
        terminate_sandbox_pod(&pool, gone_pod.id).await.expect("terminate");

        let rows = list_live_pods(&pool).await.expect("list live pods");
        assert_eq!(
            rows.iter().map(|r| r.pod_id).collect::<Vec<_>>(),
            vec![busy_pod.id, quiet_pod.id],
            "only live pods, oldest first"
        );
        let busy_row = &rows[0];
        assert_eq!(busy_row.conversation_id, busy.id);
        assert_eq!(busy_row.conversation_title, "build it");
        assert_eq!(busy_row.live_terminals, 2, "a closed terminal doesn't count");
        assert_eq!(busy_row.running_commands, 1);
        assert!(busy_row.last_command_finished_at.is_some());
        let quiet_row = &rows[1];
        assert_eq!(quiet_row.live_terminals, 0);
        assert_eq!(quiet_row.running_commands, 0);
        assert_eq!(quiet_row.last_command_finished_at, None);
        assert!(
            rows.iter().all(|r| r.observed_at >= r.created_at && r.observed_at >= r.conversation_updated_at),
            "observed_at should be the database's current time"
        );

        let with_pods = conversations_with_live_pods(&pool).await.expect("list");
        assert_eq!(with_pods, vec![busy.id, quiet.id]);
    }

    /// A pod + terminal pair, for tests that only care about
    /// `terminal_commands`/`terminal_events` and just need a valid
    /// `terminal_id` to hang them off — most of this module.
    async fn test_terminal(pool: &PgPool, conversation_id: i64) -> i64 {
        // One live pod per conversation (SME-51 B7): reuse it if there is one.
        let live = list_sandbox_pods(pool, conversation_id)
            .await
            .expect("list pods")
            .into_iter()
            .find(|p| p.terminated_at.is_none());
        let pod = match live {
            Some(pod) => pod,
            None => create_sandbox_pod(pool, conversation_id).await.expect("create sandbox pod"),
        };
        let terminal = create_sandbox_terminal(pool, pod.id)
            .await
            .expect("create sandbox terminal");
        terminal.id
    }

    #[sqlx::test]
    async fn test_create_sandbox_pod_and_terminate_is_idempotent(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create pod");
        assert!(pod.terminated_at.is_none());

        let listed = list_sandbox_pods(&pool, conversation.id)
            .await
            .expect("list pods");
        assert!(listed.iter().any(|p| p.id == pod.id));

        let terminated = terminate_sandbox_pod(&pool, pod.id)
            .await
            .expect("terminate should succeed")
            .expect("pod should exist");
        assert!(terminated.terminated_at.is_some());

        let listed_after = list_sandbox_pods(&pool, conversation.id)
            .await
            .expect("list pods");
        assert!(
            !listed_after.iter().any(|p| p.id == pod.id),
            "terminated pod should no longer be listed as live"
        );

        // Idempotent: terminating again succeeds (still finds the row),
        // doesn't error just because it's already terminated.
        let terminated_again = terminate_sandbox_pod(&pool, pod.id)
            .await
            .expect("re-terminate should succeed")
            .expect("row still exists");
        assert!(terminated_again.terminated_at.is_some());

        // Unknown pod_id: distinguishable from "already terminated" —
        // None, not an error and not a fabricated row.
        let unknown = terminate_sandbox_pod(&pool, pod.id + 999_999)
            .await
            .expect("query should succeed");
        assert!(unknown.is_none());
    }

    #[sqlx::test]
    async fn test_sandbox_terminal_lifecycle_and_pod_id_lookup(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create pod");
        let terminal = create_sandbox_terminal(&pool, pod.id)
            .await
            .expect("create terminal");

        assert_eq!(
            sandbox_terminal_pod_id(&pool, terminal.id)
                .await
                .expect("lookup"),
            Some(pod.id)
        );

        let for_pod = list_sandbox_terminals_for_pod(&pool, pod.id)
            .await
            .expect("list for pod");
        assert!(for_pod.iter().any(|t| t.id == terminal.id));
        let for_conversation = list_sandbox_terminals_for_conversation(&pool, conversation.id)
            .await
            .expect("list for conversation");
        assert!(for_conversation.iter().any(|t| t.id == terminal.id));

        terminate_sandbox_terminal(&pool, terminal.id)
            .await
            .expect("terminate should succeed")
            .expect("terminal should exist");

        let for_pod_after = list_sandbox_terminals_for_pod(&pool, pod.id)
            .await
            .expect("list for pod");
        assert!(
            !for_pod_after.iter().any(|t| t.id == terminal.id),
            "terminated terminal should no longer be listed as live"
        );
    }

    #[sqlx::test]
    async fn test_pod_previews_are_listed_per_pod_lowest_first_without_repeats(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id).await.expect("create pod");
        let elsewhere = test_conversation(&pool).await;
        let other = create_sandbox_pod(&pool, elsewhere.id).await.expect("create another pod");

        let local = |port: u16| (String::new(), port);
        add_pod_preview(&pool, pod.id, "", 5173).await.expect("add 5173");
        let after = add_pod_preview(&pool, pod.id, "", 3000).await.expect("add 3000");
        assert_eq!(after, vec![local(3000), local(5173)]);
        let again = add_pod_preview(&pool, pod.id, "", 3000).await.expect("add 3000 again");
        assert_eq!(again, vec![local(3000), local(5173)]);
        add_pod_preview(&pool, other.id, "", 8080).await.expect("add to the other pod");

        assert_eq!(list_pod_previews(&pool, pod.id).await.expect("list"), vec![local(3000), local(5173)]);
        assert_eq!(list_pod_previews(&pool, other.id).await.expect("list"), vec![local(8080)]);

        // A container's port is its own preview, next to localhost's same port (SME-33).
        let with_container = add_pod_preview(&pool, pod.id, "172.21.0.2", 3000).await.expect("add a container's 3000");
        assert_eq!(
            with_container,
            vec![local(3000), local(5173), ("172.21.0.2".to_string(), 3000)]
        );
    }

    #[sqlx::test]
    async fn test_sandbox_pod_conversation_id_resolves_and_returns_none_for_unknown(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create pod");

        assert_eq!(
            sandbox_pod_conversation_id(&pool, pod.id)
                .await
                .expect("lookup"),
            Some(conversation.id)
        );
        assert_eq!(
            sandbox_pod_conversation_id(&pool, pod.id + 999_999)
                .await
                .expect("lookup"),
            None
        );
    }

    #[sqlx::test]
    async fn test_terminating_a_pod_cascades_to_its_terminals(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create pod");
        let terminal = create_sandbox_terminal(&pool, pod.id)
            .await
            .expect("create terminal");

        terminate_sandbox_pod(&pool, pod.id)
            .await
            .expect("terminate")
            .expect("pod exists");

        // Hard DB cascade only fires on conversation deletion (see
        // SME-9's "How") — terminating the pod itself is a soft delete and
        // does *not* cascade to its terminals; the model is expected to
        // terminate_terminal each one first (enforced at the tool layer,
        // not here). Confirm the terminal row is untouched by the pod's
        // own soft delete.
        let still_there = sandbox_terminal_pod_id(&pool, terminal.id)
            .await
            .expect("lookup");
        assert_eq!(
            still_there,
            Some(pod.id),
            "terminating a pod should not itself touch its terminal rows"
        );
    }

    #[sqlx::test]
    async fn test_create_terminal_command_starts_running(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-1", "echo hi")
                .await
                .expect("create terminal command");
        assert_eq!(command.status, "running");
        assert_eq!(command.command, "echo hi");
        assert!(command.exit_code.is_none());
        assert!(command.finished_at.is_none());
        assert!(command.notified_at.is_none());
    }

    #[sqlx::test]
    async fn test_terminal_command_is_running_reflects_the_single_in_flight_row(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        assert!(
            terminal_command_is_running(&pool, terminal_id)
                .await
                .expect("query should succeed")
                .is_none(),
            "nothing running yet"
        );

        let command =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-2", "sleep 5")
                .await
                .expect("create terminal command");

        let running = terminal_command_is_running(&pool, terminal_id)
            .await
            .expect("query should succeed")
            .expect("should find the running command");
        assert_eq!(running.command_id, command.command_id);

        mark_terminal_command_finished(&pool, &command.command_id, 0)
            .await
            .expect("mark finished");
        assert!(
            terminal_command_is_running(&pool, terminal_id)
                .await
                .expect("query should succeed")
                .is_none(),
            "should no longer be running once finished"
        );
    }

    #[sqlx::test]
    async fn test_terminal_command_is_running_is_scoped_per_terminal(pool: PgPool) {
        // The whole point of moving this guard from conversation_id to
        // terminal_id: two terminals in flight at once, independently.
        let conversation = test_conversation(&pool).await;
        let terminal_a = test_terminal(&pool, conversation.id).await;
        let terminal_b = test_terminal(&pool, conversation.id).await;

        create_terminal_command(&pool, conversation.id, terminal_a, "cmd-a", "sleep 5")
            .await
            .expect("create terminal command");

        assert!(
            terminal_command_is_running(&pool, terminal_a)
                .await
                .expect("query")
                .is_some(),
            "terminal_a has a running command"
        );
        assert!(
            terminal_command_is_running(&pool, terminal_b)
                .await
                .expect("query")
                .is_none(),
            "terminal_b should be unaffected by terminal_a's running command"
        );
    }

    #[sqlx::test]
    async fn test_mark_terminal_command_finished_sets_status_and_exit_code(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-3", "false")
                .await
                .expect("create terminal command");

        mark_terminal_command_finished(&pool, &command.command_id, 1)
            .await
            .expect("mark finished");

        let status = terminal_command_status(&pool, &command.command_id)
            .await
            .expect("query should succeed")
            .expect("row should exist");
        assert_eq!(status.status, "finished");
        assert_eq!(status.exit_code, Some(1));
    }

    #[sqlx::test]
    async fn test_mark_terminal_command_lost_only_affects_running_rows(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-4", "sleep 100")
                .await
                .expect("create terminal command");
        mark_terminal_command_finished(&pool, &command.command_id, 0)
            .await
            .expect("mark finished");

        // Already finished — marking it lost afterward should be a no-op,
        // not silently overwrite a real exit code with an unknown outcome.
        mark_terminal_command_lost(&pool, &command.command_id)
            .await
            .expect("mark lost should not error");

        let status = terminal_command_status(&pool, &command.command_id)
            .await
            .expect("query should succeed")
            .expect("row should exist");
        assert_eq!(status.status, "finished");
        assert_eq!(status.exit_code, Some(0));
    }

    #[sqlx::test]
    async fn test_mark_terminal_command_lost_on_a_running_row(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-5", "sleep 100")
                .await
                .expect("create terminal command");

        mark_terminal_command_lost(&pool, &command.command_id)
            .await
            .expect("mark lost");

        let status = terminal_command_status(&pool, &command.command_id)
            .await
            .expect("query should succeed")
            .expect("row should exist");
        assert_eq!(status.status, "lost");
        assert!(status.exit_code.is_none());
    }

    #[sqlx::test]
    async fn test_append_terminal_event_and_read_terminal_output_orders_by_seq(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-6", "echo")
            .await
            .expect("create terminal command");

        // Interleaved on purpose — seq is the agent-assigned true order,
        // not insertion order, so this exercises that they can diverge.
        append_terminal_event(&pool, &command.command_id, "stdout", 1, "out1")
            .await
            .expect("append");
        append_terminal_event(&pool, &command.command_id, "stderr", 2, "err1")
            .await
            .expect("append");
        append_terminal_event(&pool, &command.command_id, "stdout", 3, "out2")
            .await
            .expect("append");

        let both = read_terminal_output(&pool, &command.command_id, &["stdout", "stderr"], 0, 10)
            .await
            .expect("read output");
        assert_eq!(
            both.iter().map(|l| l.data.as_str()).collect::<Vec<_>>(),
            vec!["out1", "err1", "out2"]
        );

        let stdout_only = read_terminal_output(&pool, &command.command_id, &["stdout"], 0, 10)
            .await
            .expect("read output");
        assert_eq!(
            stdout_only
                .iter()
                .map(|l| l.data.as_str())
                .collect::<Vec<_>>(),
            vec!["out1", "out2"]
        );

        // Same offset (0), different filter — proves line numbering is
        // relative to the requested stream set, not a stored absolute
        // position (the whole point of this design).
        assert_ne!(both[0].data, "out1".to_string().len().to_string()); // sanity: no accidental type confusion
        assert_eq!(both[0].data, "out1");
        assert_eq!(stdout_only[0].data, "out1");
        assert_eq!(both[1].data, "err1");
        // offset 1 against "both" is err1, but offset 1 against
        // "stdout" alone is out2 — different lines at the same offset.
        assert_eq!(stdout_only.get(1).map(|l| l.data.as_str()), Some("out2"));
    }

    #[sqlx::test]
    async fn test_read_terminal_output_respects_offset_and_limit(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-7", "seq 5")
                .await
                .expect("create terminal command");
        for i in 1..=5i64 {
            append_terminal_event(&pool, &command.command_id, "stdout", i, &format!("line{i}"))
                .await
                .expect("append");
        }

        let page = read_terminal_output(&pool, &command.command_id, &["stdout"], 1, 2)
            .await
            .expect("read output");
        assert_eq!(
            page.iter().map(|l| l.data.as_str()).collect::<Vec<_>>(),
            vec!["line2", "line3"]
        );
    }

    #[sqlx::test]
    async fn test_terminal_command_status_counts_lines_per_stream(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-8", "echo")
            .await
            .expect("create terminal command");
        append_terminal_event(&pool, &command.command_id, "stdout", 1, "a")
            .await
            .expect("append");
        append_terminal_event(&pool, &command.command_id, "stdout", 2, "b")
            .await
            .expect("append");
        append_terminal_event(&pool, &command.command_id, "stderr", 3, "c")
            .await
            .expect("append");

        let status = terminal_command_status(&pool, &command.command_id)
            .await
            .expect("query should succeed")
            .expect("row should exist");
        assert_eq!(status.stdout_lines, 2);
        assert_eq!(status.stderr_lines, 1);
        assert_eq!(status.status, "running");
    }

    #[sqlx::test]
    async fn test_terminal_command_status_returns_none_for_unknown_command(pool: PgPool) {
        let result = terminal_command_status(&pool, "no-such-command")
            .await
            .expect("query should succeed");
        assert!(result.is_none());
    }

    /// SME-91: a command's notice and its mark are one step, so a command
    /// already notified (a stopped drain that got as far as the mark, or
    /// another drain) never gets a second notice.
    #[sqlx::test]
    async fn test_a_command_already_notified_gets_no_second_notice(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-n1", "true")
            .await
            .expect("create");
        mark_terminal_command_finished(&pool, &command.command_id, 0)
            .await
            .expect("mark finished");
        let notice = [ContentBlock::Text { text: "cmd-n1 finished".to_string() }];

        let first = save_command_notice(&pool, conversation.id, "cmd-n1", &notice)
            .await
            .expect("first notice");
        assert!(first.is_some(), "the first notice is saved");
        let second = save_command_notice(&pool, conversation.id, "cmd-n1", &notice)
            .await
            .expect("second notice");
        assert!(second.is_none(), "a notified command gets no second notice");

        let messages = list_messages(&pool, conversation.id).await.expect("list");
        let notices = messages.iter().filter(|m| m.content.contains("cmd-n1 finished")).count();
        assert_eq!(notices, 1);
    }

    /// SME-91: the notice and the mark commit together: a command that
    /// doesn't exist (nothing to mark) leaves no notice behind.
    #[sqlx::test]
    async fn test_a_notice_without_its_mark_isnt_saved(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let notice = [ContentBlock::Text { text: "nobody's notice".to_string() }];
        let saved = save_command_notice(&pool, conversation.id, "no-such-command", &notice)
            .await
            .expect("save");
        assert!(saved.is_none());
        let messages = list_messages(&pool, conversation.id).await.expect("list");
        assert!(messages.is_empty(), "no notice without its mark: {messages:?}");
    }

    #[sqlx::test]
    async fn test_unnotified_finished_terminal_commands_matches_finished_and_lost_only(
        pool: PgPool,
    ) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let _running =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-9", "sleep 1")
                .await
                .expect("create");
        let finished =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-10", "true")
                .await
                .expect("create");
        mark_terminal_command_finished(&pool, &finished.command_id, 0)
            .await
            .expect("mark finished");
        let lost =
            create_terminal_command(&pool, conversation.id, terminal_id, "cmd-11", "sleep 100")
                .await
                .expect("create");
        mark_terminal_command_lost(&pool, &lost.command_id)
            .await
            .expect("mark lost");

        let unnotified = unnotified_finished_terminal_commands(&pool, conversation.id)
            .await
            .expect("query should succeed");
        let ids: Vec<_> = unnotified.iter().map(|c| c.command_id.as_str()).collect();
        assert!(ids.contains(&"cmd-10"));
        assert!(ids.contains(&"cmd-11"));
        assert!(
            !ids.contains(&"cmd-9"),
            "still-running command should not need notification"
        );

        mark_terminal_command_notified(&pool, &finished.command_id)
            .await
            .expect("mark notified");
        let remaining = unnotified_finished_terminal_commands(&pool, conversation.id)
            .await
            .expect("query should succeed");
        let ids: Vec<_> = remaining.iter().map(|c| c.command_id.as_str()).collect();
        assert!(!ids.contains(&"cmd-10"), "notified command should drop out");
        assert!(
            ids.contains(&"cmd-11"),
            "still-unnotified command should remain"
        );
    }

    #[sqlx::test]
    async fn test_list_terminal_commands_is_most_recent_first_and_bounded(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        for i in 1..=5 {
            create_terminal_command(
                &pool,
                conversation.id,
                terminal_id,
                &format!("list-cmd-{i}"),
                "echo",
            )
            .await
            .expect("create");
        }

        let limited = list_terminal_commands(&pool, terminal_id, 3)
            .await
            .expect("list should succeed");
        assert_eq!(limited.len(), 3);
        assert_eq!(
            limited
                .iter()
                .map(|c| c.command_id.as_str())
                .collect::<Vec<_>>(),
            vec!["list-cmd-5", "list-cmd-4", "list-cmd-3"]
        );
    }

    #[sqlx::test]
    async fn test_delete_conversation_cascades_to_terminal_commands_and_events(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command = create_terminal_command(
            &pool,
            conversation.id,
            terminal_id,
            "cmd-cascade",
            "echo hi",
        )
        .await
        .expect("create terminal command");
        append_terminal_event(&pool, &command.command_id, "stdout", 1, "hi")
            .await
            .expect("append");

        delete_conversation(&pool, conversation.id)
            .await
            .expect("delete conversation");

        let status = terminal_command_status(&pool, &command.command_id)
            .await
            .expect("query should succeed");
        assert!(
            status.is_none(),
            "terminal_commands (and its events, cascading further) should be gone with the conversation"
        );
    }

    #[sqlx::test]
    async fn test_terminating_a_terminal_preserves_its_command_history(pool: PgPool) {
        // The whole reason terminate_terminal soft-deletes instead of
        // DELETEing (see SME-9's "How"): list_commands should still be
        // able to show what ran in a terminal that's since been torn down.
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create pod");
        let terminal = create_sandbox_terminal(&pool, pod.id)
            .await
            .expect("create terminal");
        let command = create_terminal_command(
            &pool,
            conversation.id,
            terminal.id,
            "cmd-survives",
            "echo hi",
        )
        .await
        .expect("create terminal command");
        mark_terminal_command_finished(&pool, &command.command_id, 0)
            .await
            .expect("mark finished");

        terminate_sandbox_terminal(&pool, terminal.id)
            .await
            .expect("terminate should succeed")
            .expect("terminal should exist");

        let status = terminal_command_status(&pool, &command.command_id)
            .await
            .expect("query should succeed");
        assert!(
            status.is_some(),
            "command history should survive terminate_terminal"
        );

        let history = list_terminal_commands(&pool, terminal.id, 10)
            .await
            .expect("list should still work against a terminated terminal");
        assert!(history.iter().any(|c| c.command_id == command.command_id));
    }

    #[sqlx::test]
    async fn test_create_list_get_delete_mcp_server_config_round_trip(pool: PgPool) {
        let headers = HashMap::from([("Authorization".to_string(), "Bearer secret".to_string())]);
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://api.githubcopilot.com/mcp/",
            &headers,
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");
        assert_eq!(created.name, "github");
        assert_eq!(created.extra_headers.0, headers);

        let all = list_mcp_server_configs(&pool)
            .await
            .expect("list mcp server configs");
        assert!(all.iter().any(|s| s.id == created.id));

        let fetched = get_mcp_server_config(&pool, created.id)
            .await
            .expect("get mcp server config")
            .expect("row should exist");
        assert_eq!(fetched.url, "https://api.githubcopilot.com/mcp/");

        delete_mcp_server_config(&pool, created.id)
            .await
            .expect("delete mcp server config");
        let after_delete = get_mcp_server_config(&pool, created.id)
            .await
            .expect("get should still succeed");
        assert!(after_delete.is_none(), "row should be gone after delete");
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_renames_without_touching_headers(pool: PgPool) {
        let original_headers = HashMap::from([(
            "Authorization".to_string(),
            "Bearer original-token".to_string(),
        )]);
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &original_headers,
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let updated = update_mcp_server_config(
            &pool,
            created.id,
            "github-renamed",
            "https://example.com/mcp-v2",
            &HashMap::new(),
            &[],
            "static_headers",
        )
        .await
        .expect("update should succeed")
        .expect("row should exist");

        assert_eq!(updated.name, "github-renamed");
        assert_eq!(updated.url, "https://example.com/mcp-v2");
        assert_eq!(
            updated.extra_headers.0, original_headers,
            "an empty upsert/remove must never touch stored headers"
        );
    }

    #[sqlx::test]
    async fn test_get_mcp_server_config_by_name_round_trips(pool: PgPool) {
        let headers = HashMap::new();
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://api.githubcopilot.com/mcp/",
            &headers,
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let fetched = get_mcp_server_config_by_name(&pool, "github")
            .await
            .expect("query should succeed")
            .expect("row should exist");
        assert_eq!(fetched.id, created.id);

        let missing = get_mcp_server_config_by_name(&pool, "does-not-exist")
            .await
            .expect("query should succeed");
        assert!(missing.is_none());
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_upserts_headers_without_touching_others(pool: PgPool) {
        let original = HashMap::from([
            ("Authorization".to_string(), "Bearer old".to_string()),
            ("X-Untouched".to_string(), "keep-me".to_string()),
        ]);
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &original,
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let upsert = HashMap::from([
            ("Authorization".to_string(), "Bearer new".to_string()),
            ("X-New".to_string(), "brand-new".to_string()),
        ]);
        let updated = update_mcp_server_config(
            &pool,
            created.id,
            "github",
            "https://example.com/mcp",
            &upsert,
            &[],
            "static_headers",
        )
        .await
        .expect("update should succeed")
        .expect("row should exist");

        assert_eq!(
            updated.extra_headers.0,
            HashMap::from([
                ("Authorization".to_string(), "Bearer new".to_string()),
                ("X-Untouched".to_string(), "keep-me".to_string()),
                ("X-New".to_string(), "brand-new".to_string()),
            ]),
            "upsert should update/add given keys and leave every other existing header alone"
        );
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_removes_named_headers(pool: PgPool) {
        let original = HashMap::from([
            ("Authorization".to_string(), "Bearer old".to_string()),
            ("X-Doomed".to_string(), "bye".to_string()),
        ]);
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &original,
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let updated = update_mcp_server_config(
            &pool,
            created.id,
            "github",
            "https://example.com/mcp",
            &HashMap::new(),
            &["X-Doomed".to_string()],
            "static_headers",
        )
        .await
        .expect("update should succeed")
        .expect("row should exist");

        assert_eq!(
            updated.extra_headers.0,
            HashMap::from([("Authorization".to_string(), "Bearer old".to_string())]),
            "the removed header must be gone, everything else untouched"
        );
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_upsert_wins_over_remove_for_the_same_name(pool: PgPool) {
        let original = HashMap::from([("Authorization".to_string(), "Bearer old".to_string())]);
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &original,
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let upsert = HashMap::from([("Authorization".to_string(), "Bearer new".to_string())]);
        let updated = update_mcp_server_config(
            &pool,
            created.id,
            "github",
            "https://example.com/mcp",
            &upsert,
            &["Authorization".to_string()],
            "static_headers",
        )
        .await
        .expect("update should succeed")
        .expect("row should exist");

        assert_eq!(
            updated.extra_headers.0,
            HashMap::from([("Authorization".to_string(), "Bearer new".to_string())]),
            "a name present in both upsert and remove should end up set, not removed"
        );
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_returns_none_for_unknown_id(pool: PgPool) {
        let result = update_mcp_server_config(
            &pool,
            999_999,
            "name",
            "https://example.com",
            &HashMap::new(),
            &[],
            "static_headers",
        )
        .await
        .expect("query should succeed");
        assert!(result.is_none());
    }

    #[sqlx::test]
    async fn test_ssh_key_round_trips_and_deletes(pool: PgPool) {
        let work = create_ssh_key(&pool, "work", "ssh-ed25519 AAAA work", "PRIVATE-W")
            .await
            .expect("create key");
        let listed = list_ssh_keys(&pool).await.expect("list keys");
        assert_eq!(listed, vec![work.clone()]);
        assert_eq!(listed[0].private_key, "PRIVATE-W");

        delete_ssh_key(&pool, work.id).await.expect("delete key");
        assert!(list_ssh_keys(&pool).await.expect("list keys").is_empty());
    }

    #[sqlx::test]
    async fn test_git_identity_is_empty_until_set_then_updates_in_place(pool: PgPool) {
        assert_eq!(
            get_git_identity(&pool).await.expect("get"),
            crate::git::GitIdentity::default()
        );
        let first = crate::git::GitIdentity {
            name: "Ada".into(),
            email: "ada@example.com".into(),
        };
        set_git_identity(&pool, &first).await.expect("set");
        assert_eq!(get_git_identity(&pool).await.expect("get"), first);

        let second = crate::git::GitIdentity {
            name: "Ada Lovelace".into(),
            email: "ada@lovelace.dev".into(),
        };
        set_git_identity(&pool, &second).await.expect("set again");
        assert_eq!(get_git_identity(&pool).await.expect("get"), second);
    }

    #[sqlx::test]
    async fn test_conversation_repos_round_trip_through_a_clone(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("create conversation");
        let repo = create_conversation_repo(
            &pool,
            conversation.id,
            "git@github.com:o/r.git",
            "github.com/o/r",
            Some("dev"),
            "r",
        )
        .await
        .expect("create repo");
        assert_eq!(repo.status, "cloning");
        assert_eq!(repo.branch.as_deref(), Some("dev"));

        set_repo_failed(&pool, repo.id, repo.attempt, "fatal: nope").await.expect("fail");
        let listed = list_conversation_repos(&pool, conversation.id).await.expect("list");
        assert_eq!((listed[0].status.as_str(), listed[0].error.as_deref()), ("failed", Some("fatal: nope")));

        let retried = retry_repo_clone(&pool, repo.id, "ssh://git@github.com/o/r", "github.com/o/r", None)
            .await
            .expect("retry")
            .expect("it was failed");
        assert_eq!((retried.status.as_str(), retried.error.as_deref()), ("cloning", None));
        assert_eq!((retried.url.as_str(), retried.branch.as_deref()), ("ssh://git@github.com/o/r", None));
        set_repo_cloned(&pool, repo.id, "dev", Some("abc123")).await.expect("cloned");
        let listed = list_conversation_repos(&pool, conversation.id).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "ready");
        assert_eq!(listed[0].error, None);
        assert_eq!(listed[0].checked_out_branch.as_deref(), Some("dev"));
        assert_eq!(listed[0].commit_sha.as_deref(), Some("abc123"));

        // After a restart, a clone still marked cloning was cut off.
        let other = create_conversation(&pool).await.expect("conversation");
        let unfinished = create_conversation_repo(&pool, other.id, "u", "k", None, "x")
            .await
            .expect("repo");
        assert_eq!(fail_unfinished_clones(&pool, "interrupted").await.expect("sweep"), 1);
        let row = get_conversation_repo(&pool, unfinished.id).await.expect("get").expect("exists");
        assert_eq!((row.status.as_str(), row.error.as_deref()), ("failed", Some("interrupted")));
        let untouched = get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
        assert_eq!(untouched.status, "ready", "a finished clone is left alone");

        // The checkout's AGENTS.md files.
        set_repo_agents_files(&pool, repo.id, &["AGENTS.md".to_string(), "web/AGENTS.md".to_string()])
            .await
            .expect("agents files");
        let row = get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
        assert_eq!(row.agents_files, vec!["AGENTS.md".to_string(), "web/AGENTS.md".to_string()]);

        // Loading an AGENTS.md, and loading it again replaces it.
        let file = |content: &str| InstructionsFile {
            content: content.to_string(),
            file_bytes: content.len() as i64,
            hash: format!("hash-{content}"),
            commit: Some("abc123".to_string()),
        };
        load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("v1")).await.expect("load");
        load_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("v2")).await.expect("reload");
        load_instruction(&pool, conversation.id, repo.id, "web/AGENTS.md", &file("web")).await.expect("load nested");
        let loaded = list_loaded_instructions(&pool, conversation.id).await.expect("list loaded");
        let paths: Vec<(&str, &str)> = loaded.iter().map(|l| (l.path.as_str(), l.content.as_str())).collect();
        assert_eq!(paths, vec![("AGENTS.md", "v2"), ("web/AGENTS.md", "web")]);
        assert_eq!(loaded[0].commit_sha.as_deref(), Some("abc123"));

        // A request waiting on trust; asking again replaces it.
        request_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("seen")).await.expect("request");
        let request = request_instruction(&pool, conversation.id, repo.id, "AGENTS.md", &file("newer"))
            .await
            .expect("request again");
        let requests = list_instruction_requests(&pool, conversation.id).await.expect("list requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].file(), file("newer"));
        assert_eq!(
            list_instruction_requests_for_remote(&pool, "github.com/o/r").await.expect("by remote"),
            requests
        );
        assert_eq!(get_instruction_request(&pool, request.id).await.expect("get"), Some(request.clone()));
        delete_instruction_request(&pool, request.id).await.expect("delete request");
        assert!(list_instruction_requests(&pool, conversation.id).await.expect("list").is_empty());

        // Trust decisions, remembered per remote either way.
        assert_eq!(get_repo_trust(&pool, "github.com/o/r").await.expect("get"), None);
        set_repo_trust(&pool, "github.com/o/r", false).await.expect("decline");
        set_repo_trust(&pool, "github.com/o/r", true).await.expect("trust");
        assert_eq!(get_repo_trust(&pool, "github.com/o/r").await.expect("get"), Some(true));
        assert_eq!(list_repo_trust(&pool).await.expect("list").len(), 1);
        delete_repo_trust(&pool, "github.com/o/r").await.expect("forget");
        assert_eq!(get_repo_trust(&pool, "github.com/o/r").await.expect("get"), None);

        // One checkout per directory.
        assert!(
            create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
                .await
                .is_err()
        );
        // Deleting the conversation deletes its repos.
        delete_conversation(&pool, conversation.id).await.expect("delete conversation");
        assert!(list_conversation_repos(&pool, conversation.id).await.expect("list").is_empty());
    }

    /// Only a failed clone is retried, and only the current attempt can
    /// finish a clone: a stale attempt's late "ready" or "failed" changes
    /// nothing (SME-86).
    #[sqlx::test]
    async fn test_only_the_current_attempt_finishes_a_clone(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("conversation");
        let repo = create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
            .await
            .expect("create repo");
        assert_eq!(repo.attempt, 1);
        let status = |pool: PgPool| async move {
            let row = get_conversation_repo(&pool, repo.id).await.expect("get").expect("exists");
            (row.status, row.attempt)
        };

        // A clone that's still running isn't retried.
        assert!(retry_repo_clone(&pool, repo.id, "u", "k", None).await.expect("retry").is_none());
        assert_eq!(status(pool.clone()).await, ("cloning".to_string(), 1));

        // Attempt 1 finishes; its guard's late "failed" changes nothing.
        assert!(set_repo_ready(&pool, repo.id, 1, "main", None, &[], None).await.expect("ready"));
        assert!(!set_repo_failed(&pool, repo.id, 1, "interrupted").await.expect("late failed"));
        assert_eq!(status(pool.clone()).await, ("ready".to_string(), 1));
        // A ready clone isn't retried either.
        assert!(retry_repo_clone(&pool, repo.id, "u", "k", None).await.expect("retry").is_none());

        // Another clone's failed attempt 1 is retried as attempt 2;
        // attempt 1's late writes don't touch it.
        let failed = create_conversation_repo(&pool, conversation.id, "u", "k", None, "f")
            .await
            .expect("create repo");
        assert!(set_repo_failed(&pool, failed.id, 1, "fatal: nope").await.expect("fail"));
        let retried = retry_repo_clone(&pool, failed.id, "u", "k", None)
            .await
            .expect("retry")
            .expect("it was failed");
        assert_eq!((retried.status.as_str(), retried.attempt), ("cloning", 2));
        assert!(!set_repo_failed(&pool, failed.id, 1, "interrupted").await.expect("stale failed"));
        assert!(!set_repo_ready(&pool, failed.id, 1, "main", None, &[], None).await.expect("stale ready"));
        let row = get_conversation_repo(&pool, failed.id).await.expect("get").expect("exists");
        assert_eq!((row.status.as_str(), row.attempt), ("cloning", 2));
        assert!(set_repo_ready(&pool, failed.id, 2, "main", None, &[], None).await.expect("ready"));
        let row = get_conversation_repo(&pool, failed.id).await.expect("get").expect("exists");
        assert_eq!(row.status, "ready");
    }

    /// Two retries of the same failed clone at once: exactly one runs.
    #[sqlx::test]
    async fn test_two_retries_of_a_failed_clone_one_wins(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("conversation");
        let repo = create_conversation_repo(&pool, conversation.id, "u", "k", None, "r")
            .await
            .expect("create repo");
        assert!(set_repo_failed(&pool, repo.id, 1, "fatal: nope").await.expect("fail"));
        let (a, b) = tokio::join!(
            retry_repo_clone(&pool, repo.id, "u", "k", None),
            retry_repo_clone(&pool, repo.id, "u", "k", None),
        );
        let won = [a.expect("retry a"), b.expect("retry b")].into_iter().flatten().count();
        assert_eq!(won, 1, "exactly one retry runs");
    }

    #[sqlx::test]
    async fn test_create_sandbox_volume_round_trips_name_and_mount_path(pool: PgPool) {
        let created = create_sandbox_volume(&pool, "ssh-key", "/home/sandbox/.ssh")
            .await
            .expect("create sandbox volume");
        assert_eq!(created.name, "ssh-key");
        assert_eq!(created.mount_path, "/home/sandbox/.ssh");

        let fetched = get_sandbox_volume(&pool, created.id)
            .await
            .expect("get sandbox volume")
            .expect("should exist");
        assert_eq!(fetched, created);
    }

    #[sqlx::test]
    async fn test_get_sandbox_volume_returns_none_for_unknown_id(pool: PgPool) {
        let result = get_sandbox_volume(&pool, 999_999)
            .await
            .expect("query should succeed");
        assert!(result.is_none());
    }

    #[sqlx::test]
    async fn test_list_sandbox_volumes_orders_by_name(pool: PgPool) {
        create_sandbox_volume(&pool, "zzz-cache", "/data/cache")
            .await
            .expect("create volume");
        create_sandbox_volume(&pool, "aaa-ssh", "/home/sandbox/.ssh")
            .await
            .expect("create volume");

        let listed = list_sandbox_volumes(&pool)
            .await
            .expect("list sandbox volumes");
        let names: Vec<&str> = listed.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["aaa-ssh", "zzz-cache"]);
    }

    #[sqlx::test]
    async fn test_delete_sandbox_volume_actually_removes_it(pool: PgPool) {
        let created = create_sandbox_volume(&pool, "build-cache", "/data/cache")
            .await
            .expect("create volume");
        delete_sandbox_volume(&pool, created.id)
            .await
            .expect("delete sandbox volume");

        let fetched = get_sandbox_volume(&pool, created.id)
            .await
            .expect("query should succeed");
        assert!(
            fetched.is_none(),
            "deleted volume should no longer be gettable"
        );
    }

    #[sqlx::test]
    async fn test_ensure_mcp_server_inserts_a_missing_server_once(pool: PgPool) {
        let inserted = ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
            .await
            .expect("ensure mcp server");
        assert!(inserted, "the first call should insert");
        let again = ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
            .await
            .expect("ensure mcp server again");
        assert!(!again, "the second call should insert nothing");

        let configs = list_mcp_server_configs(&pool).await.expect("list");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "exa");
        assert_eq!(configs[0].url, "https://mcp.example.com/mcp");
        assert_eq!(configs[0].auth_mode, "static_headers");
        assert!(configs[0].extra_headers.0.is_empty());
    }

    #[sqlx::test]
    async fn test_ensure_mcp_server_leaves_an_edited_entry_alone(pool: PgPool) {
        let headers = HashMap::from([("x-api-key".to_string(), "my-key".to_string())]);
        let existing = create_mcp_server_config(
            &pool,
            "exa",
            "https://edited.example.com/mcp",
            &headers,
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let inserted = ensure_mcp_server(&pool, "exa", "https://mcp.example.com/mcp")
            .await
            .expect("ensure mcp server");
        assert!(!inserted);

        let after = get_mcp_server_config(&pool, existing.id)
            .await
            .expect("get")
            .expect("the entry should still exist");
        assert_eq!(after.url, "https://edited.example.com/mcp");
        assert_eq!(after.extra_headers.0, headers);
        assert_eq!(after.updated_at, existing.updated_at);
    }

    #[sqlx::test]
    async fn test_create_mcp_server_config_defaults_auth_mode_to_static_headers(pool: PgPool) {
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &HashMap::new(),
            "static_headers",
            None,
            None,
        )
        .await
        .expect("create mcp server config");
        assert_eq!(created.auth_mode, "static_headers");
        assert!(created.oauth_credentials.is_none());
    }

    #[sqlx::test]
    async fn test_create_mcp_server_config_stores_a_preregistered_oauth_client(pool: PgPool) {
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://api.githubcopilot.com/mcp/",
            &HashMap::new(),
            "oauth",
            Some("client-123"),
            Some("secret-456"),
        )
        .await
        .expect("create mcp server config");
        assert_eq!(created.oauth_client_id.as_deref(), Some("client-123"));
        assert_eq!(created.oauth_client_secret.as_deref(), Some("secret-456"));
    }

    #[sqlx::test]
    async fn test_create_mcp_server_config_oauth_mode_round_trips(pool: PgPool) {
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &HashMap::new(),
            "oauth",
            None,
            None,
        )
        .await
        .expect("create mcp server config");
        assert_eq!(created.auth_mode, "oauth");
    }

    #[sqlx::test]
    async fn test_set_mcp_server_oauth_credentials_round_trips(pool: PgPool) {
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &HashMap::new(),
            "oauth",
            None,
            None,
        )
        .await
        .expect("create mcp server config");

        let credentials =
            serde_json::json!({"client_id": "abc", "token_response": null, "granted_scopes": []});
        set_mcp_server_oauth_credentials(&pool, created.id, Some(credentials.clone()))
            .await
            .expect("set oauth credentials");

        let fetched = get_mcp_server_config(&pool, created.id)
            .await
            .expect("get mcp server config")
            .expect("row should exist");
        assert_eq!(fetched.oauth_credentials.map(|j| j.0), Some(credentials));

        set_mcp_server_oauth_credentials(&pool, created.id, None)
            .await
            .expect("clear oauth credentials");
        let cleared = get_mcp_server_config(&pool, created.id)
            .await
            .expect("get mcp server config")
            .expect("row should exist");
        assert!(
            cleared.oauth_credentials.is_none(),
            "None should clear the stored credentials"
        );
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_clears_oauth_credentials_when_url_changes(pool: PgPool) {
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &HashMap::new(),
            "oauth",
            None,
            None,
        )
        .await
        .expect("create mcp server config");
        set_mcp_server_oauth_credentials(
            &pool,
            created.id,
            Some(serde_json::json!({"client_id": "abc"})),
        )
        .await
        .expect("set oauth credentials");

        // Changing the URL invalidates a grant that was issued for the old
        // audience — see SME-16's "Data model."
        let updated = update_mcp_server_config(
            &pool,
            created.id,
            "github",
            "https://example.com/mcp-v2",
            &HashMap::new(),
            &[],
            "oauth",
        )
        .await
        .expect("update should succeed")
        .expect("row should exist");

        assert!(
            updated.oauth_credentials.is_none(),
            "a URL change must clear stored OAuth credentials"
        );
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_keeps_oauth_credentials_when_url_is_unchanged(
        pool: PgPool,
    ) {
        let created = create_mcp_server_config(
            &pool,
            "github",
            "https://example.com/mcp",
            &HashMap::new(),
            "oauth",
            None,
            None,
        )
        .await
        .expect("create mcp server config");
        let credentials = serde_json::json!({"client_id": "abc"});
        set_mcp_server_oauth_credentials(&pool, created.id, Some(credentials.clone()))
            .await
            .expect("set oauth credentials");

        let updated = update_mcp_server_config(
            &pool,
            created.id,
            "github-renamed",
            "https://example.com/mcp",
            &HashMap::new(),
            &[],
            "oauth",
        )
        .await
        .expect("update should succeed")
        .expect("row should exist");

        assert_eq!(
            updated.oauth_credentials.map(|j| j.0),
            Some(credentials),
            "renaming without changing the URL must not disturb stored OAuth credentials"
        );
    }

    // --- Model providers (SME-72) ---

    async fn test_provider(pool: &PgPool, name: &str) -> InferenceProvider {
        create_inference_provider(pool, name, "anthropic", "https://api.anthropic.com", "api_key", "sk-ant-0123456789")
            .await
            .expect("create provider")
    }

    /// Mechanical CRUD, mirroring the MCP server table's: a characterization
    /// round trip rather than test-first.
    #[sqlx::test]
    async fn test_inference_provider_round_trip_keeps_the_secret_unless_replaced(pool: PgPool) {
        let created = test_provider(&pool, "anthropic").await;
        assert_eq!(list_inference_providers(&pool).await.expect("list"), vec![created.clone()]);

        let renamed = update_inference_provider(&pool, created.id, "work", "other", "https://gw.example", "bearer", None)
            .await
            .expect("update")
            .expect("exists");
        assert_eq!(
            (renamed.name.as_str(), renamed.kind.as_str(), renamed.base_url.as_str(), renamed.auth_kind.as_str()),
            ("work", "other", "https://gw.example", "bearer")
        );
        assert_eq!(renamed.secret, "sk-ant-0123456789", "no new secret keeps the stored one");

        let rekeyed = update_inference_provider(&pool, created.id, "work", "other", "https://gw.example", "bearer", Some("new-secret"))
            .await
            .expect("update")
            .expect("exists");
        assert_eq!(rekeyed.secret, "new-secret");

        assert_eq!(
            update_inference_provider(&pool, 999_999, "x", "other", "u", "bearer", None).await.expect("update"),
            None
        );
    }

    #[sqlx::test]
    async fn test_a_providers_debug_hides_its_secret(pool: PgPool) {
        let provider = test_provider(&pool, "p").await;
        let shown = format!("{provider:?}");
        assert!(!shown.contains("sk-ant-0123456789"), "{shown}");
        assert!(shown.contains("anthropic"), "{shown}");
    }

    #[sqlx::test]
    async fn test_deleting_a_provider_clears_the_default_and_conversations_using_it(pool: PgPool) {
        let doomed = test_provider(&pool, "doomed").await;
        let kept = test_provider(&pool, "kept").await;
        set_default_model(&pool, doomed.id, "m1").await.expect("default");
        let on_doomed = create_conversation(&pool).await.expect("conversation");
        let on_kept = create_conversation(&pool).await.expect("conversation");
        set_conversation_model(&pool, on_doomed.id, doomed.id, "m1").await.expect("set");
        set_conversation_model(&pool, on_kept.id, kept.id, "m2").await.expect("set");

        assert!(delete_inference_provider(&pool, doomed.id).await.expect("delete"));

        assert_eq!(get_inference_provider(&pool, doomed.id).await.expect("get"), None);
        assert_eq!(get_default_model(&pool).await.expect("default"), None);
        let cleared = get_conversation_model(&pool, on_doomed.id).await.expect("get").expect("exists");
        assert_eq!((cleared.provider_id, cleared.model), (None, None));
        let untouched = get_conversation_model(&pool, on_kept.id).await.expect("get").expect("exists");
        assert_eq!((untouched.provider_id, untouched.model.as_deref()), (Some(kept.id), Some("m2")));
        assert!(!delete_inference_provider(&pool, doomed.id).await.expect("delete again"));
    }

    #[sqlx::test]
    async fn test_deleting_a_provider_keeps_another_providers_default(pool: PgPool) {
        let doomed = test_provider(&pool, "doomed").await;
        let kept = test_provider(&pool, "kept").await;
        set_default_model(&pool, kept.id, "m2").await.expect("default");

        assert!(delete_inference_provider(&pool, doomed.id).await.expect("delete"));

        assert_eq!(get_default_model(&pool).await.expect("default"), Some((kept.id, "m2".to_string())));
    }

    #[sqlx::test]
    async fn test_setting_a_conversations_model_leaves_the_switch_point_to_its_next_turn(pool: PgPool) {
        let provider = test_provider(&pool, "p").await;
        let conversation = create_conversation(&pool).await.expect("conversation");
        let text = [ContentBlock::Text { text: "hi".to_string() }];
        create_message(&pool, conversation.id, "user", &text).await.expect("message");

        assert!(set_conversation_model(&pool, conversation.id, provider.id, "m1").await.expect("set"));
        let set = get_conversation_model(&pool, conversation.id).await.expect("get").expect("exists");
        assert_eq!(
            set,
            ConversationModelRow {
                provider_id: Some(provider.id),
                model: Some("m1".to_string()),
                model_changed_at_message_id: None,
            },
            "a running turn may still write for the old model, so the pick doesn't mark the switch"
        );
        assert!(!set_conversation_model(&pool, 999_999, provider.id, "m1").await.expect("set"));
    }

    /// SME-72 review: the switch point is where a turn starts on a
    /// different backend than the last turn, recorded under the turn lock.
    #[sqlx::test]
    async fn test_a_turn_on_a_different_backend_records_the_switch_point(pool: PgPool) {
        let conversation = create_conversation(&pool).await.expect("conversation");
        let text = [ContentBlock::Text { text: "hi".to_string() }];
        let first = create_message(&pool, conversation.id, "user", &text).await.expect("message");

        assert_eq!(record_turn_model(&pool, conversation.id, "1 http://a m1").await.expect("record"), Some(first.id));
        let reply = create_message(&pool, conversation.id, "assistant", &text).await.expect("message");
        assert_eq!(
            record_turn_model(&pool, conversation.id, "1 http://a m1").await.expect("record"),
            Some(first.id),
            "the same backend again: unchanged"
        );
        assert_eq!(
            record_turn_model(&pool, conversation.id, "1 http://b m1").await.expect("record"),
            Some(reply.id),
            "another base URL is another backend"
        );
    }

    #[sqlx::test]
    async fn test_setting_a_deleted_providers_model_is_refused(pool: PgPool) {
        let provider = test_provider(&pool, "p").await;
        let conversation = create_conversation(&pool).await.expect("conversation");
        delete_inference_provider(&pool, provider.id).await.expect("delete");

        assert!(set_conversation_model(&pool, conversation.id, provider.id, "m1").await.is_err());
        assert!(set_default_model(&pool, provider.id, "m1").await.is_err());
    }

    #[sqlx::test]
    async fn test_a_conversation_adopts_the_default_only_when_it_has_no_model(pool: PgPool) {
        let provider = test_provider(&pool, "p").await;
        let conversation = create_conversation(&pool).await.expect("conversation");

        assert_eq!(adopt_default_model(&pool, conversation.id).await.expect("adopt"), None, "no default yet");

        set_default_model(&pool, provider.id, "m1").await.expect("default");
        assert_eq!(
            adopt_default_model(&pool, conversation.id).await.expect("adopt"),
            Some((provider.id, "m1".to_string()))
        );
        let adopted = get_conversation_model(&pool, conversation.id).await.expect("get").expect("exists");
        assert_eq!((adopted.provider_id, adopted.model.as_deref()), (Some(provider.id), Some("m1")));

        set_default_model(&pool, provider.id, "m2").await.expect("default");
        assert_eq!(adopt_default_model(&pool, conversation.id).await.expect("adopt"), None, "it has one now");
        let kept = get_conversation_model(&pool, conversation.id).await.expect("get").expect("exists");
        assert_eq!(kept.model.as_deref(), Some("m1"));
    }

    /// SME-72 review 2: Ollama reports a window from `/api/ps` only while
    /// the model is loaded; asking again when it isn't keeps what it said.
    #[sqlx::test]
    async fn test_a_report_without_a_window_keeps_the_last_one(pool: PgPool) {
        let provider = test_provider(&pool, "p").await;
        set_provider_model_reported(&pool, provider.id, "m", Some(8192), Some(true), Some(true)).await.expect("loaded");
        set_provider_model_reported(&pool, provider.id, "m", None, Some(false), Some(true)).await.expect("unloaded");
        let row = get_provider_model(&pool, provider.id, "m").await.expect("get").expect("exists");
        assert_eq!((row.reported_context_window, row.reported_thinking), (Some(8192), Some(false)));
    }

    #[sqlx::test]
    async fn test_model_overrides_and_reported_details_leave_each_other_alone(pool: PgPool) {
        let provider = test_provider(&pool, "p").await;
        set_provider_model_reported(&pool, provider.id, "m", Some(4096), Some(false), Some(true)).await.expect("reported");
        set_provider_model_overrides(&pool, provider.id, "m", Some(true), Some(32_768)).await.expect("overrides");
        set_provider_model_reported(&pool, provider.id, "m", Some(8192), None, Some(true)).await.expect("reported");

        let row = get_provider_model(&pool, provider.id, "m").await.expect("get").expect("exists");
        assert_eq!(
            row,
            ProviderModelRow {
                provider_id: provider.id,
                model: "m".to_string(),
                thinking: Some(true),
                context_window: Some(32_768),
                reported_context_window: Some(8192),
                reported_thinking: None,
                reported_tools: Some(true),
                added_by_hand: false,
            }
        );
        assert_eq!(list_provider_models(&pool, provider.id).await.expect("list"), vec![row]);
    }

    /// SME-99: the browser tier's ids start clear of every other run's,
    /// for each table whose ids name cluster objects.
    #[sqlx::test]
    async fn test_ids_start_clear_of_other_runs(pool: PgPool) {
        let base = test_support::start_ids_clear_of_other_runs(&pool).await.expect("move the id sequences");
        assert!(base >= 2_000_000_000, "base {base} is in the range other runs use");

        let conversation = create_conversation(&pool).await.expect("conversation");
        let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
        let volume = create_sandbox_volume(&pool, "sme-99-volume", "/data").await.expect("volume row");
        for (what, id) in [("conversation", conversation.id), ("sandbox pod", pod.id), ("sandbox volume", volume.id)] {
            assert!(id > base, "the next {what} id is {id}, not above the base {base}");
        }

        let again = test_support::start_ids_clear_of_other_runs(&pool).await.expect("move them again");
        assert_ne!(again, base, "two runs got the same base");
    }
}

/// Database helpers for tests.
#[cfg(all(test, feature = "server"))]
pub(crate) mod test_support {
    use sqlx::PgPool;

    /// Where `start_ids_clear_of_other_runs` puts the id sequences: far
    /// above the low ids every fresh `#[sqlx::test]` database starts at,
    /// and above the 1,000,000+ range some unit tests move their own pod
    /// ids to.
    const CLEAR_IDS_BASE: i64 = 2_000_000_000;

    /// Moves the conversation, sandbox pod and sandbox volume id
    /// sequences to a base of their own, different on every call, and
    /// returns it. Cluster objects are named after these ids (pod labels
    /// and claims after the conversation, `sandbox-{id}` pods, volume
    /// claims), and the browser tier shares its namespace with the
    /// real-cluster unit tests, whose databases also count from 1: a run
    /// that met a pod an earlier run was still stopping waited it out
    /// and failed (SME-99).
    pub(crate) async fn start_ids_clear_of_other_runs(pool: &PgPool) -> Result<i64, sqlx::Error> {
        use std::time::{SystemTime, UNIX_EPOCH};
        // Nanoseconds since the epoch, folded into a billion-wide window
        // above the base: a fresh base every call, and still far inside
        // BIGINT and anything a name or a browser's number can hold.
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
        let base = CLEAR_IDS_BASE + i64::try_from(nanos % 1_000_000_000).unwrap_or_default();
        for table in ["conversations", "sandbox_pods", "sandbox_volumes"] {
            sqlx::query("SELECT setval(pg_get_serial_sequence($1, 'id'), $2)")
                .bind(table)
                .bind(base)
                .execute(pool)
                .await?;
        }
        Ok(base)
    }
}
