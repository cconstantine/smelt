//! `message_agent`'s transaction (SME-238): a parent and its child write
//! to each other through the `agent_messages` queue. Lock order as in
//! `super`: `conversations` rows in ascending id, then the recipient's
//! `agents` row.

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
    queue_agent_message(&mut tx, recipient, Some(caller), label, KIND_MESSAGE, text).await?;
    tx.commit().await?;
    Ok(MessageOutcome::Queued(recipient))
}
