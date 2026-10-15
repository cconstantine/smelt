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
    #[expect(clippy::expect_used, reason = "startup: smelt can't run without its database")]
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    #[expect(clippy::expect_used, reason = "startup: smelt can't run without its database")]
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

    #[expect(clippy::expect_used, reason = "a second db::init() is a programming error")]
    POOL.set(pool).expect("Database already initialized");
    get()
}

#[expect(clippy::expect_used, reason = "db::get() before db::init() is a programming error, by design")]
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

/// The saved price catalog (SME-106): when it was fetched, and
/// `pricing::Providers` as JSON.
pub async fn get_price_catalog(
    pool: &PgPool,
) -> Result<Option<(NaiveDateTime, serde_json::Value)>, sqlx::Error> {
    sqlx::query_as("SELECT fetched_at, providers FROM price_catalog").fetch_optional(pool).await
}

/// Replaces the saved price catalog.
pub async fn save_price_catalog(
    pool: &PgPool,
    fetched_at: NaiveDateTime,
    providers: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO price_catalog (id, fetched_at, providers) VALUES (TRUE, $1, $2)
         ON CONFLICT (id) DO UPDATE SET fetched_at = EXCLUDED.fetched_at, providers = EXCLUDED.providers",
    )
    .bind(fetched_at)
    .bind(providers)
    .execute(pool)
    .await?;
    Ok(())
}

/// Each provider's models' calls since `since`, costliest first (SME-106).
/// A deleted provider's calls are kept, grouped under no provider.
pub async fn model_spend_since(
    pool: &PgPool,
    since: NaiveDateTime,
) -> Result<Vec<crate::models::ModelSpend>, sqlx::Error> {
    sqlx::query_as::<_, crate::models::ModelSpend>(
        "SELECT p.name AS provider_name, u.model,
                count(*) AS calls,
                sum(u.input_tokens)::BIGINT AS input_tokens,
                sum(u.output_tokens)::BIGINT AS output_tokens,
                sum(u.cache_creation_input_tokens)::BIGINT AS cache_creation_input_tokens,
                sum(u.cache_read_input_tokens)::BIGINT AS cache_read_input_tokens,
                sum(u.cost_usd) AS cost_usd,
                count(*) FILTER (WHERE u.cost_usd IS NULL) AS unpriced_calls
         FROM model_call_usage u
         LEFT JOIN inference_providers p ON p.id = u.provider_id
         WHERE u.created_at >= $1
         GROUP BY u.provider_id, p.name, u.model
         ORDER BY sum(u.cost_usd) DESC NULLS LAST, count(*) DESC, p.name, u.model",
    )
    .bind(since)
    .fetch_all(pool)
    .await
}

/// Which request a `model_call_usage` row is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCallKind {
    Turn,
    Compaction,
}

impl ModelCallKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Compaction => "compaction",
        }
    }
}

/// One completed model call, for `record_model_call`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCall<'a> {
    pub conversation_id: i64,
    pub provider_id: i64,
    pub model: &'a str,
    pub kind: ModelCallKind,
    pub usage: crate::anthropic::TokenUsage,
    /// From the price catalog when the call finished; `None` without a price.
    pub cost_usd: Option<f64>,
}

/// Appends `call` to `model_call_usage` (SME-106). A turn's call is also
/// the conversation's new last-known usage (`upsert_conversation_usage`),
/// written in the same transaction so the two never disagree. A
/// compaction's isn't: it described the old, long history.
pub async fn record_model_call(pool: &PgPool, call: &ModelCall<'_>) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        // The provider as it is now: a turn reads it when it starts, and the
        // user may delete it before the reply finishes. A deleted one gives
        // null, as the deletion's own SET NULL would (code review 1). `FOR
        // KEY SHARE` waits for a delete still in progress and then reads
        // null; without it the id is read, and the foreign key fails once
        // the delete commits (code review 2).
        "INSERT INTO model_call_usage
             (conversation_id, provider_id, model, kind, input_tokens, output_tokens,
              cache_creation_input_tokens, cache_read_input_tokens, cost_usd)
         VALUES ($1, (SELECT id FROM inference_providers WHERE id = $2 FOR KEY SHARE),
                 $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(call.conversation_id)
    .bind(call.provider_id)
    .bind(call.model)
    .bind(call.kind.as_str())
    .bind(call.usage.input_tokens)
    .bind(call.usage.output_tokens)
    .bind(call.usage.cache_creation_input_tokens)
    .bind(call.usage.cache_read_input_tokens)
    .bind(call.cost_usd)
    .execute(&mut *tx)
    .await?;
    if call.kind == ModelCallKind::Turn {
        upsert_conversation_usage(&mut *tx, call.conversation_id, &call.usage).await?;
    }
    tx.commit().await
}

pub async fn get_conversation_spend(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<crate::models::ConversationSpend, sqlx::Error> {
    sqlx::query_as::<_, crate::models::ConversationSpend>(
        "SELECT count(*) AS calls,
                COALESCE(sum(input_tokens), 0)::BIGINT AS input_tokens,
                COALESCE(sum(output_tokens), 0)::BIGINT AS output_tokens,
                COALESCE(sum(cache_creation_input_tokens), 0)::BIGINT AS cache_creation_input_tokens,
                COALESCE(sum(cache_read_input_tokens), 0)::BIGINT AS cache_read_input_tokens,
                sum(cost_usd) AS cost_usd,
                count(*) FILTER (WHERE cost_usd IS NULL) AS unpriced_calls
         FROM model_call_usage WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
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

// --- This database's identity in the cluster (SME-115) ---

/// Which database a cluster object belongs to. Every smelt server shares
/// the sandbox namespace, and its objects are named after this database's
/// ids, so each one is labelled `smelt/instance=<id>` and a server only
/// deletes, reuses, mounts or watches objects carrying its own id.
#[derive(Clone, Debug, PartialEq)]
pub struct SmeltInstance {
    /// A UUID, made when the database was migrated.
    pub id: String,
    /// Whether this database owns the objects made before SME-115, which
    /// carry no instance label: true only for a database that already had
    /// conversations or volumes when it migrated (the dev and production
    /// databases), fixed then. Only such a database adopts them.
    pub owns_unlabelled: bool,
}

/// The migration that made `smelt_instance` (its version).
#[cfg(test)]
const SMELT_INSTANCE_MIGRATION: i64 = 20261007200000;

/// This database's `smelt_instance` row.
pub async fn smelt_instance(pool: &PgPool) -> Result<SmeltInstance, sqlx::Error> {
    let (id, owns_unlabelled): (String, bool) =
        sqlx::query_as("SELECT instance_id::text, owns_unlabelled FROM smelt_instance")
            .fetch_one(pool)
            .await?;
    Ok(SmeltInstance { id, owns_unlabelled })
}

/// Starts a fresh database's conversation, sandbox pod and sandbox volume
/// ids at a high base of their own (SME-115), and returns it; `None`, and
/// nothing changed, for a database any of them was ever used in (the dev
/// and production databases). Cluster objects are named after these ids,
/// and every smelt server shares the namespace: a scratch database
/// counting from 1 would name its claims and pods like the dev server's.
pub async fn start_ids_clear_if_fresh(pool: &PgPool) -> Result<Option<i64>, sqlx::Error> {
    // Each sequence's last value: null until its first one.
    let sequences: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT t.name, s.last_value
         FROM unnest($1::text[]) AS t(name)
         JOIN pg_sequences s ON format('%I.%I', s.schemaname, s.sequencename) = pg_get_serial_sequence(t.name, 'id')",
    )
    .bind(&CLEAR_ID_TABLES[..])
    .fetch_all(pool)
    .await?;
    // Any id ever handed out below the base: a used database, left alone.
    // One already at or above it was moved by an earlier start; a move
    // that failed partway (`setval` isn't transactional) is finished now
    // (SME-115 review 2).
    if sequences.len() != CLEAR_ID_TABLES.len()
        || sequences.iter().any(|(_, last)| last.is_some_and(|last| last < CLEAR_IDS_BASE))
    {
        return Ok(None);
    }
    let unused: Vec<&str> = sequences.iter().filter(|(_, last)| last.is_none()).map(|(name, _)| name.as_str()).collect();
    if unused.is_empty() {
        return Ok(None);
    }
    let base = clear_ids_base();
    for table in unused {
        sqlx::query("SELECT setval(pg_get_serial_sequence($1, 'id'), $2)")
            .bind(table)
            .bind(base)
            .execute(pool)
            .await?;
    }
    Ok(Some(base))
}

/// The tables whose ids name cluster objects: claims and pod labels after
/// the conversation, `sandbox-{id}` pods, volume claims.
const CLEAR_ID_TABLES: [&str; 3] = ["conversations", "sandbox_pods", "sandbox_volumes"];

/// Far above the low ids a database counts from (the dev database is in
/// the thousands, every fresh `#[sqlx::test]` one starts at 1, and some
/// unit tests move their own pod ids to the millions).
const CLEAR_IDS_BASE: i64 = 2_000_000_000;

/// `CLEAR_IDS_BASE` plus nanoseconds since the epoch folded into a
/// billion-wide window: a fresh base every call, and still far inside
/// BIGINT and anything a name or a browser's number can hold.
fn clear_ids_base() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    CLEAR_IDS_BASE + i64::try_from(nanos % 1_000_000_000).unwrap_or_default()
}

/// Moves `CLEAR_ID_TABLES`' id sequences to `base`: the next id is above it.
#[cfg(test)]
async fn move_id_sequences(pool: &PgPool, base: i64) -> Result<(), sqlx::Error> {
    for table in CLEAR_ID_TABLES {
        sqlx::query("SELECT setval(pg_get_serial_sequence($1, 'id'), $2)")
            .bind(table)
            .bind(base)
            .execute(pool)
            .await?;
    }
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
    /// Mark turn requests for prompt caching (SME-106).
    pub prompt_caching: bool,
    /// The models.dev provider that prices its calls (SME-106).
    pub price_catalog_provider: Option<String>,
    /// llama.cpp only: send the template's `preserve_thinking` (SME-111).
    pub keep_reasoning: bool,
    /// llama.cpp only: its `/props`, as `providers::LlamaServerInfo`.
    pub server_caps: Option<serde_json::Value>,
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
            .field("prompt_caching", &self.prompt_caching)
            .field("price_catalog_provider", &self.price_catalog_provider)
            .field("keep_reasoning", &self.keep_reasoning)
            .field("server_caps", &self.server_caps)
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
    prompt_caching: bool,
    price_catalog_provider: Option<&str>,
    keep_reasoning: bool,
) -> Result<InferenceProvider, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>(
        "INSERT INTO inference_providers (name, kind, base_url, auth_kind, secret, prompt_caching, price_catalog_provider, keep_reasoning)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *",
    )
    .bind(name)
    .bind(kind)
    .bind(base_url)
    .bind(auth_kind)
    .bind(secret)
    .bind(prompt_caching)
    .bind(price_catalog_provider)
    .bind(keep_reasoning)
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
    prompt_caching: bool,
    price_catalog_provider: Option<&str>,
    keep_reasoning: bool,
) -> Result<Option<InferenceProvider>, sqlx::Error> {
    sqlx::query_as::<_, InferenceProvider>(
        "UPDATE inference_providers
         SET name = $2, kind = $3, base_url = $4, auth_kind = $5,
             secret = COALESCE($6, secret), prompt_caching = $7, price_catalog_provider = $8,
             keep_reasoning = $9,
             -- Another server's caps don't describe this one (SME-111 review 2).
             server_caps = CASE WHEN kind = $3 AND base_url = $4 THEN server_caps END,
             updated_at = now()
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(kind)
    .bind(base_url)
    .bind(auth_kind)
    .bind(secret)
    .bind(prompt_caching)
    .bind(price_catalog_provider)
    .bind(keep_reasoning)
    .fetch_optional(pool)
    .await
}

/// Keeps what a llama.cpp server last said about itself (SME-111).
pub async fn set_inference_provider_server_caps(
    pool: &PgPool,
    id: i64,
    server_caps: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE inference_providers SET server_caps = $2 WHERE id = $1")
        .bind(id)
        .bind(server_caps)
        .execute(pool)
        .await?;
    Ok(())
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
    /// The user's effort, `anthropic::Effort` as text (SME-106).
    pub effort: Option<String>,
    /// The user's cap on a reply (SME-111).
    pub max_output: Option<i32>,
    /// The provider's cap on a reply, as last reported.
    pub reported_max_output: Option<i32>,
    /// The user's cap on a reply's thinking, llama.cpp only (SME-111).
    pub reasoning_budget: Option<i32>,
    /// Whether the model takes adaptive thinking, as last reported (B9,
    /// SME-122).
    pub reported_adaptive_thinking: Option<bool>,
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
    effort: Option<&str>,
    max_output: Option<i32>,
    reasoning_budget: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models (provider_id, model, thinking, context_window, effort, max_output, reasoning_budget)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (provider_id, model) DO UPDATE
         SET thinking = $3, context_window = $4, effort = $5, max_output = $6, reasoning_budget = $7",
    )
    .bind(provider_id)
    .bind(model)
    .bind(thinking)
    .bind(context_window)
    .bind(effort)
    .bind(max_output)
    .bind(reasoning_budget)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces what the provider reported about a model, leaving the user's
/// overrides alone. A report without a context window keeps the last one:
/// Ollama only says while the model is loaded. The same goes for a reply
/// cap (SME-111).
pub async fn set_provider_model_reported(
    pool: &PgPool,
    provider_id: i64,
    model: &str,
    context_window: Option<i32>,
    thinking: Option<bool>,
    tools: Option<bool>,
    max_output: Option<i32>,
    adaptive_thinking: Option<bool>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_models
             (provider_id, model, reported_context_window, reported_thinking, reported_tools, reported_max_output,
              reported_adaptive_thinking)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (provider_id, model) DO UPDATE
         SET reported_context_window = COALESCE($3, provider_models.reported_context_window),
             reported_thinking = $4, reported_tools = $5,
             reported_max_output = COALESCE($6, provider_models.reported_max_output),
             reported_adaptive_thinking = $7",
    )
    .bind(provider_id)
    .bind(model)
    .bind(context_window)
    .bind(thinking)
    .bind(tools)
    .bind(max_output)
    .bind(adaptive_thinking)
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
mod tests;

/// Database helpers for tests.
#[cfg(all(test, feature = "server"))]
pub(crate) mod test_support {
    use sqlx::PgPool;

    /// Moves the conversation, sandbox pod and sandbox volume id
    /// sequences to a base of their own, different on every call, and
    /// returns it, used or not (unlike `start_ids_clear_if_fresh`, which
    /// `main` runs). The browser tier shares its namespace with the
    /// real-cluster unit tests, whose databases also count from 1: a run
    /// that met a pod an earlier run was still stopping waited it out
    /// and failed (SME-99).
    pub(crate) async fn start_ids_clear_of_other_runs(pool: &PgPool) -> Result<i64, sqlx::Error> {
        let base = super::clear_ids_base();
        super::move_id_sequences(pool, base).await?;
        Ok(base)
    }
}
