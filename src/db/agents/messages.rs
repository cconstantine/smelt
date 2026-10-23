//! `message_agent`'s and `ask_parent`'s transactions (SME-238): a parent
//! and its child write to each other through the `agent_messages` queue,
//! and a child's question to its parent is a `pending_questions` row the
//! parent's next message answers. Lock order as in `super`:
//! `conversations` rows in ascending id, then an `agents` row, then the
//! question.

use super::*;

/// Who `message_agent` writes to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Recipient {
    /// The caller's parent.
    Parent,
    /// One of the caller's children, by id.
    Child(i64),
}

/// `message_agent`'s outcome.
#[derive(Debug, PartialEq)]
pub enum MessageOutcome {
    /// Queued to this conversation; the caller wakes it.
    Queued(i64),
    /// It answered this child's question to its parent; the caller wakes it.
    Answered(i64),
    /// The caller has no parent.
    NoParent,
    /// The id isn't one of the caller's children.
    NotYourChild(i64),
    /// The recipient is deleted, or being deleted.
    Deleted(i64),
    /// The recipient is an agent that takes no messages: `stopped` or
    /// `failed`.
    Ended(i64, String),
}

/// `message_agent`'s transaction: the recipient checked and the message
/// queued to it, so a stop or a delete either lands first (and refuses it)
/// or waits for it. A `finished` recipient is made `active` here, under
/// its `agents` row's lock. `label` names the caller to the recipient.
pub async fn message_agent(
    pool: &PgPool,
    caller: i64,
    to: Recipient,
    label: &str,
    text: &str,
) -> Result<MessageOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let recipient = match to {
        Recipient::Parent => {
            let parent = sqlx::query_scalar::<_, Option<i64>>("SELECT parent_id FROM conversations WHERE id = $1")
                .bind(caller)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
            match parent {
                Some(parent) => parent,
                None => return Ok(MessageOutcome::NoParent),
            }
        }
        Recipient::Child(id) => {
            let mine = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM conversations WHERE id = $1 AND parent_id = $2)",
            )
            .bind(id)
            .bind(caller)
            .fetch_one(&mut *tx)
            .await?;
            if !mine {
                return Ok(MessageOutcome::NotYourChild(id));
            }
            id
        }
    };
    if !lock_conversations(&mut tx, &[(caller, RowLock::KeyShare), (recipient, RowLock::KeyShare)]).await? {
        return Ok(MessageOutcome::Deleted(recipient));
    }
    // `FOR NO KEY UPDATE`, not `FOR SHARE`: a finished recipient is updated
    // below, and two messages upgrading a shared lock would deadlock. A
    // stop, a `finish` or a delete's marking updates this row, so each
    // lands wholly before this read or waits for the commit.
    let state = sqlx::query_scalar::<_, String>("SELECT state FROM agents WHERE conversation_id = $1 FOR NO KEY UPDATE")
        .bind(recipient)
        .fetch_optional(&mut *tx)
        .await?;
    // Read after the row lock, so a delete's marking that committed first
    // is seen.
    let deleting = sqlx::query_scalar::<_, bool>("SELECT deleting_at IS NOT NULL FROM conversations WHERE id = $1")
        .bind(recipient)
        .fetch_one(&mut *tx)
        .await?;
    if deleting {
        return Ok(MessageOutcome::Deleted(recipient));
    }
    match state.as_deref() {
        Some(state @ (AGENT_STOPPED | AGENT_FAILED)) => return Ok(MessageOutcome::Ended(recipient, state.to_string())),
        Some(AGENT_FINISHED) => {
            // SME-245 adds its "agents at once" check here.
            sqlx::query(
                "UPDATE agents SET state = $2, state_changed_at = now() WHERE conversation_id = $1 AND state = $3",
            )
            .bind(recipient)
            .bind(AGENT_ACTIVE)
            .bind(AGENT_FINISHED)
            .execute(&mut *tx)
            .await?;
        }
        _ => {}
    }
    // A child waiting for its parent: this message is the answer. The
    // `answer IS NULL` re-check after a concurrent take (the user's
    // message) finds no row, and the message is queued instead.
    if matches!(to, Recipient::Child(_)) {
        let answer = [crate::questions::QuestionAnswer { selected: Vec::new(), other: Some(text.to_string()) }];
        let answered = sqlx::query(
            "UPDATE pending_questions SET answer = $2
             WHERE conversation_id = $1 AND asked_of = $3 AND answer IS NULL",
        )
        .bind(recipient)
        .bind(sqlx::types::Json(&answer[..]))
        .bind(super::super::ASKED_OF_PARENT)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if answered > 0 {
            tx.commit().await?;
            return Ok(MessageOutcome::Answered(recipient));
        }
    }
    queue_agent_message(&mut tx, recipient, Some(caller), label, KIND_MESSAGE, text).await?;
    tx.commit().await?;
    Ok(MessageOutcome::Queued(recipient))
}

/// `ask_parent`'s outcome.
#[derive(Debug, PartialEq)]
pub enum AskParent {
    /// Asked; the question is queued to this parent, which the caller wakes.
    Asked { parent: i64 },
    /// The caller has no parent.
    NoParent,
    /// The caller isn't an `active` agent: its state, `None` when it's gone.
    NotActive(Option<String>),
    /// The parent takes no questions: deleted, or a stopped or failed agent.
    ParentEnded(String),
}

/// `ask_parent`'s transaction: the child's question recorded as asked of
/// its parent (`tool_use_id`, so its answer becomes the call's result) and
/// queued to the parent, both or neither. The child's `agents` row is held
/// `FOR SHARE`, so a `stop_agent` lands wholly before (and refuses it) or
/// after. `label` names the child to its parent.
pub async fn ask_parent(
    pool: &PgPool,
    child: i64,
    tool_use_id: &str,
    question: &str,
    label: &str,
) -> Result<AskParent, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(parent) = sqlx::query_scalar::<_, Option<i64>>("SELECT parent_id FROM conversations WHERE id = $1")
        .bind(child)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
    else {
        return Ok(AskParent::NoParent);
    };
    if !lock_conversations(&mut tx, &[(parent, RowLock::KeyShare), (child, RowLock::KeyShare)]).await? {
        return Ok(AskParent::NotActive(None));
    }
    let state = sqlx::query_scalar::<_, String>("SELECT state FROM agents WHERE conversation_id = $1 FOR SHARE")
        .bind(child)
        .fetch_optional(&mut *tx)
        .await?;
    if state.as_deref() != Some(AGENT_ACTIVE) {
        return Ok(AskParent::NotActive(state));
    }
    let (deleting, parent_state) = sqlx::query_as::<_, (bool, Option<String>)>(
        "SELECT c.deleting_at IS NOT NULL, a.state FROM conversations c
         LEFT JOIN agents a ON a.conversation_id = c.id WHERE c.id = $1",
    )
    .bind(parent)
    .fetch_one(&mut *tx)
    .await?;
    if deleting {
        return Ok(AskParent::ParentEnded("deleted".to_string()));
    }
    if let Some(state @ (AGENT_STOPPED | AGENT_FAILED)) = parent_state.as_deref() {
        return Ok(AskParent::ParentEnded(state.to_string()));
    }
    let asked = [crate::questions::Question {
        question: question.to_string(),
        header: "Question for your parent".to_string(),
        options: Vec::new(),
        multi_select: false,
    }];
    super::super::upsert_pending_question(&mut tx, child, tool_use_id, &asked, super::super::ASKED_OF_PARENT).await?;
    queue_agent_message(&mut tx, parent, Some(child), label, KIND_QUESTION, question).await?;
    tx.commit().await?;
    Ok(AskParent::Asked { parent })
}
