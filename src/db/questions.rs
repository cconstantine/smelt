use super::*;

// --- Pending questions (the model's ask_user, SME-34) ---

/// `pending_questions.asked_of`: the user (`ask_user`, the card).
pub const ASKED_OF_USER: &str = "user";
/// `pending_questions.asked_of`: the child's parent (`ask_parent`, SME-238).
pub const ASKED_OF_PARENT: &str = "parent";

/// A conversation's `pending_questions` row: the question it waits on, and
/// the answer once given (`None` while waiting), and who it's asked of.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredQuestion {
    pub tool_use_id: String,
    pub questions: Vec<crate::questions::Question>,
    pub answer: Option<Vec<crate::questions::QuestionAnswer>>,
    /// `ASKED_OF_USER` or `ASKED_OF_PARENT`.
    pub asked_of: String,
}

/// Records the question `conversation_id` now waits on, replacing any
/// earlier one (there is none: a turn only runs once the last is taken).
pub async fn create_pending_question(
    pool: &PgPool,
    conversation_id: i64,
    tool_use_id: &str,
    questions: &[crate::questions::Question],
) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    upsert_pending_question(&mut conn, conversation_id, tool_use_id, questions, ASKED_OF_USER).await
}

/// `create_pending_question`'s upsert, asked of `asked_of`, on the
/// caller's connection or transaction.
pub async fn upsert_pending_question(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    tool_use_id: &str,
    questions: &[crate::questions::Question],
    asked_of: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pending_questions (conversation_id, tool_use_id, questions, asked_of)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (conversation_id) DO UPDATE SET
             tool_use_id = EXCLUDED.tool_use_id,
             questions = EXCLUDED.questions,
             asked_of = EXCLUDED.asked_of,
             answer = NULL,
             created_at = now()",
    )
    .bind(conversation_id)
    .bind(tool_use_id)
    .bind(sqlx::types::Json(questions))
    .bind(asked_of)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

type StoredQuestionRow = (
    String,
    sqlx::types::Json<Vec<crate::questions::Question>>,
    Option<sqlx::types::Json<Vec<crate::questions::QuestionAnswer>>>,
    String,
);

fn stored_question((tool_use_id, questions, answer, asked_of): StoredQuestionRow) -> StoredQuestion {
    StoredQuestion {
        tool_use_id,
        questions: questions.0,
        answer: answer.map(|a| a.0),
        asked_of,
    }
}

/// The question `conversation_id` waits on or has an answer for, if any.
pub async fn get_pending_question(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Option<StoredQuestion>, sqlx::Error> {
    let row = sqlx::query_as::<_, StoredQuestionRow>(
        "SELECT tool_use_id, questions, answer, asked_of FROM pending_questions WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(stored_question))
}

/// Records the user's `answer` for the waiting question `tool_use_id`.
/// False when there's no such question, it was already answered (another
/// tab got there first) or it's asked of the child's parent (SME-238):
/// exactly one answer is ever recorded.
pub async fn answer_pending_question(
    pool: &PgPool,
    conversation_id: i64,
    tool_use_id: &str,
    answer: &[crate::questions::QuestionAnswer],
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE pending_questions SET answer = $3
         WHERE conversation_id = $1 AND tool_use_id = $2 AND answer IS NULL AND asked_of = $4",
    )
    .bind(conversation_id)
    .bind(tool_use_id)
    .bind(sqlx::types::Json(answer))
    .bind(ASKED_OF_USER)
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
         RETURNING tool_use_id, questions, answer, asked_of",
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

/// Conversations with a question still waiting for the user's answer,
/// for the sidebar's waiting mark: not a child waiting for its parent.
pub async fn list_waiting_conversations(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT conversation_id FROM pending_questions WHERE answer IS NULL AND asked_of = $1 ORDER BY conversation_id",
    )
    .bind(ASKED_OF_USER)
    .fetch_all(pool)
    .await
}
