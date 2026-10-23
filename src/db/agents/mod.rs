//! Child agents' rows (SME-236): a child's `agents` row and the
//! `agent_messages` queue that carries reports and notices to a
//! conversation. The lifecycle around them is `crate::agents`.
//!
//! Lock order, in every transaction here: the remote's trust lock, if any;
//! then `conversations` rows in ascending id (`lock_conversations`); then
//! everything else. A parent's id is always smaller than its children's.

use super::*;

mod delete;
mod start;

pub use delete::*;
pub use start::*;

/// `agents.state`: the start is running; no turn is admitted.
pub const AGENT_STARTING: &str = "starting";
/// `agents.state`: started, working or between turns.
pub const AGENT_ACTIVE: &str = "active";
/// `agents.state`: it called `finish`.
pub const AGENT_FINISHED: &str = "finished";
/// `agents.state`: `stop_agent` or a delete stopped it.
pub const AGENT_STOPPED: &str = "stopped";
/// `agents.state`: its start failed; terminal.
pub const AGENT_FAILED: &str = "failed";

/// `agent_messages.kind`: a child's report to its parent.
pub const KIND_REPORT: &str = "report";
/// `agent_messages.kind`: a notice from smelt.
pub const KIND_NOTICE: &str = "notice";

/// One queued report or notice, as the drain reads it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AgentMessage {
    pub id: i64,
    pub to_conversation_id: i64,
    pub from_label: String,
    pub kind: String,
    pub body: String,
}

/// A row lock on a `conversations` row.
#[derive(Clone, Copy, Debug)]
pub enum RowLock {
    KeyShare,
    NoKeyUpdate,
    Update,
}

impl RowLock {
    fn sql(self) -> &'static str {
        match self {
            RowLock::KeyShare => "SELECT id FROM conversations WHERE id = $1 FOR KEY SHARE",
            RowLock::NoKeyUpdate => "SELECT id FROM conversations WHERE id = $1 FOR NO KEY UPDATE",
            RowLock::Update => "SELECT id FROM conversations WHERE id = $1 FOR UPDATE",
        }
    }
}

/// Locks `conversations` rows in ascending id, each in its own mode, the
/// order every transaction here takes them in. Returns whether every row
/// still exists.
pub async fn lock_conversations(
    conn: &mut sqlx::PgConnection,
    rows: &[(i64, RowLock)],
) -> Result<bool, sqlx::Error> {
    let mut rows = rows.to_vec();
    rows.sort_by_key(|(id, _)| *id);
    let mut all = true;
    for (id, lock) in rows {
        all &= sqlx::query(lock.sql()).bind(id).fetch_optional(&mut *conn).await?.is_some();
    }
    Ok(all)
}

/// The conversation's parent, `None` for the user's own conversations
/// and for one that doesn't exist.
pub async fn conversation_parent(pool: &PgPool, id: i64) -> Result<Option<i64>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT parent_id FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .flatten())
}

/// The agent's `state`, `None` for a conversation that isn't an agent.
pub async fn agent_state(pool: &PgPool, id: i64) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT state FROM agents WHERE conversation_id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Queues a report or notice to `to`, on the caller's transaction.
pub async fn queue_agent_message(
    conn: &mut sqlx::PgConnection,
    to: i64,
    from: Option<i64>,
    from_label: &str,
    kind: &str,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_messages (to_conversation_id, from_conversation_id, from_label, kind, body)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(to)
    .bind(from)
    .bind(from_label)
    .bind(kind)
    .bind(body)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `finish_agent`'s outcome.
#[derive(Debug, PartialEq)]
pub enum Finish {
    /// The report is queued to this parent.
    Queued { parent: i64 },
    /// The agent isn't `active`: its state, or `None` when it isn't an
    /// agent or is gone.
    NotActive(Option<String>),
}

/// `finish`'s transaction: the child `finished` and its report queued to
/// its parent, so one lands only if the other does. `label` names the
/// child to its parent.
pub async fn finish_agent(pool: &PgPool, child: i64, label: &str, report: &str) -> Result<Finish, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(parent) = sqlx::query_scalar::<_, Option<i64>>("SELECT parent_id FROM conversations WHERE id = $1")
        .bind(child)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
    else {
        return Ok(Finish::NotActive(None));
    };
    if !lock_conversations(&mut tx, &[(parent, RowLock::KeyShare), (child, RowLock::KeyShare)]).await? {
        return Ok(Finish::NotActive(None));
    }
    let finished = sqlx::query(
        "UPDATE agents SET state = $2, state_changed_at = now() WHERE conversation_id = $1 AND state = $3",
    )
    .bind(child)
    .bind(AGENT_FINISHED)
    .bind(AGENT_ACTIVE)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    #[cfg(test)]
    crate::agents::test_hooks::reach(crate::agents::test_hooks::FINISH_UPDATED, child).await;
    if finished == 0 {
        let state = sqlx::query_scalar::<_, String>("SELECT state FROM agents WHERE conversation_id = $1")
            .bind(child)
            .fetch_optional(&mut *tx)
            .await?;
        return Ok(Finish::NotActive(state));
    }
    queue_agent_message(&mut tx, parent, Some(child), label, KIND_REPORT, report).await?;
    tx.commit().await?;
    Ok(Finish::Queued { parent })
}

/// `stop_subtree`'s outcome.
#[derive(Debug, PartialEq)]
pub enum StopSubtree {
    /// `id` isn't one of the caller's children.
    NotYourChild,
    /// It had already ended, in this state: nothing changed.
    AlreadyEnded(String),
    /// It and everything under it are stopped; these are their ids, `id` first.
    Stopped(Vec<i64>),
}

/// The ids of `id` and every conversation under it, `id` first, then in
/// ascending id.
async fn subtree_ids(conn: &mut sqlx::PgConnection, id: i64) -> Result<Vec<i64>, sqlx::Error> {
    let mut ids = sqlx::query_scalar::<_, i64>(
        "WITH RECURSIVE sub(id) AS (
             SELECT id FROM conversations WHERE id = $1
             UNION ALL
             SELECT c.id FROM conversations c JOIN sub ON c.parent_id = sub.id
         )
         SELECT id FROM sub WHERE id <> $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    ids.insert(0, id);
    Ok(ids)
}

/// `stop_agent`'s transaction: `id`, one of `caller`'s children, and its
/// whole subtree set `stopped`, the rows locked in ascending id first. The
/// caller stops their turns after the commit (state first, SME-217's order).
pub async fn stop_subtree(pool: &PgPool, caller: i64, id: i64) -> Result<StopSubtree, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let is_child = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM agents a JOIN conversations c ON c.id = a.conversation_id
                        WHERE a.conversation_id = $1 AND c.parent_id = $2)",
    )
    .bind(id)
    .bind(caller)
    .fetch_one(&mut *tx)
    .await?;
    if !is_child {
        return Ok(StopSubtree::NotYourChild);
    }
    let subtree = subtree_ids(&mut tx, id).await?;
    let locks: Vec<(i64, RowLock)> = subtree.iter().map(|id| (*id, RowLock::KeyShare)).collect();
    lock_conversations(&mut tx, &locks).await?;
    let state = sqlx::query_scalar::<_, String>("SELECT state FROM agents WHERE conversation_id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    match state.as_deref() {
        None => return Ok(StopSubtree::NotYourChild),
        Some(state @ (AGENT_STOPPED | AGENT_FAILED)) => return Ok(StopSubtree::AlreadyEnded(state.to_string())),
        Some(_) => {}
    }
    sqlx::query(
        "UPDATE agents SET state = $2, state_changed_at = now()
         WHERE conversation_id = ANY($1) AND state IN ($3, $4, $5)",
    )
    .bind(&subtree)
    .bind(AGENT_STOPPED)
    .bind(AGENT_STARTING)
    .bind(AGENT_ACTIVE)
    .bind(AGENT_FINISHED)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StopSubtree::Stopped(subtree))
}

/// The user's send or answer in a `finished` or `stopped` child that had
/// started: it's `active` again. `false` when it isn't in either (a child
/// stopped during its start never had `started_at`, so it stays stopped).
pub async fn reactivate_agent(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE agents SET state = $2, state_changed_at = now()
         WHERE conversation_id = $1 AND state IN ($3, $4) AND started_at IS NOT NULL",
    )
    .bind(id)
    .bind(AGENT_ACTIVE)
    .bind(AGENT_FINISHED)
    .bind(AGENT_STOPPED)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// The user's turn, holding the turn lock, in an agent that `finish`ed
/// while it waited: `active` again. Only `finished`: a stop wins.
pub async fn reactivate_finished(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE agents SET state = $2, state_changed_at = now() WHERE conversation_id = $1 AND state = $3",
    )
    .bind(id)
    .bind(AGENT_ACTIVE)
    .bind(AGENT_FINISHED)
    .execute(pool)
    .await?;
    Ok(())
}

/// Queues a notice from smelt to `to` about agent `from` (`from_label`),
/// in a transaction of its own: one that reports no state change.
pub async fn queue_notice(pool: &PgPool, to: i64, from: i64, from_label: &str, body: &str) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    queue_agent_message(&mut conn, to, Some(from), from_label, KIND_NOTICE, body).await
}

/// Whether `id` is an `active` agent with nothing to wait on: no question
/// to the user unanswered, no AGENTS.md card, no terminal command running
/// and no agent of its own still starting or active. Its parent then, and
/// the first 500 characters of its last reply's text. `None` otherwise.
pub async fn idle_agent_without_report(pool: &PgPool, id: i64) -> Result<Option<(i64, String)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT c.parent_id,
                (SELECT m.content FROM messages m WHERE m.conversation_id = c.id AND m.role = 'assistant'
                 ORDER BY m.id DESC LIMIT 1)
         FROM conversations c JOIN agents a ON a.conversation_id = c.id
         WHERE c.id = $1 AND c.parent_id IS NOT NULL AND a.state = $2
           AND NOT EXISTS (SELECT 1 FROM pending_questions q WHERE q.conversation_id = c.id AND q.answer IS NULL)
           AND NOT EXISTS (SELECT 1 FROM instruction_requests r WHERE r.conversation_id = c.id)
           AND NOT EXISTS (SELECT 1 FROM terminal_commands t WHERE t.conversation_id = c.id AND t.status = 'running')
           AND NOT EXISTS (SELECT 1 FROM agents g JOIN conversations gc ON gc.id = g.conversation_id
                           WHERE gc.parent_id = c.id AND g.state IN ($3, $2))",
    )
    .bind(id)
    .bind(AGENT_ACTIVE)
    .bind(AGENT_STARTING)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(parent, content)| {
        let text = content
            .and_then(|content| serde_json::from_str::<Vec<ContentBlock>>(&content).ok())
            .map(|blocks| {
                blocks
                    .into_iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        (parent, text.chars().take(500).collect())
    }))
}

/// One of a conversation's children, as `list_agents` shows it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ChildAgent {
    pub id: i64,
    pub title: String,
    pub persona_path: Option<String>,
    pub state: String,
    pub created_at: NaiveDateTime,
    pub last_message_at: Option<NaiveDateTime>,
    /// How many agents it has started itself.
    pub agents_started: i64,
    /// A question to the user is unanswered, or an AGENTS.md trust card waits.
    pub waiting_for_user: bool,
}

/// `parent`'s own children, newest first: never anyone else's.
pub async fn list_child_agents(pool: &PgPool, parent: i64) -> Result<Vec<ChildAgent>, sqlx::Error> {
    sqlx::query_as::<_, ChildAgent>(
        "SELECT c.id, c.title, a.persona_path, a.state, a.created_at,
                (SELECT max(m.created_at) FROM messages m WHERE m.conversation_id = c.id) AS last_message_at,
                (SELECT count(*) FROM conversations g WHERE g.parent_id = c.id) AS agents_started,
                (EXISTS (SELECT 1 FROM pending_questions q WHERE q.conversation_id = c.id AND q.answer IS NULL)
                 OR EXISTS (SELECT 1 FROM instruction_requests r WHERE r.conversation_id = c.id)) AS waiting_for_user
         FROM conversations c JOIN agents a ON a.conversation_id = c.id
         WHERE c.parent_id = $1
         ORDER BY c.id DESC",
    )
    .bind(parent)
    .fetch_all(pool)
    .await
}

/// Every conversation with an undelivered report or notice that isn't a
/// `stopped` agent: the startup sweep wakes them.
pub async fn conversations_with_queued_messages(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT DISTINCT m.to_conversation_id FROM agent_messages m
         LEFT JOIN agents a ON a.conversation_id = m.to_conversation_id
         WHERE m.delivered_at IS NULL AND (a.state IS NULL OR a.state <> $1)
         ORDER BY m.to_conversation_id",
    )
    .bind(AGENT_STOPPED)
    .fetch_all(pool)
    .await
}

/// Deletes every AGENTS.md trust card in `conversation_id`; returns how many.
pub async fn delete_instruction_requests_of(pool: &PgPool, conversation_id: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM instruction_requests WHERE conversation_id = $1")
        .bind(conversation_id)
        .execute(pool)
        .await?
        .rows_affected())
}

/// Every undelivered report or notice to `to`, oldest first.
pub async fn undelivered_agent_messages(pool: &PgPool, to: i64) -> Result<Vec<AgentMessage>, sqlx::Error> {
    sqlx::query_as::<_, AgentMessage>(
        "SELECT id, to_conversation_id, from_label, kind, body FROM agent_messages
         WHERE to_conversation_id = $1 AND delivered_at IS NULL ORDER BY id",
    )
    .bind(to)
    .fetch_all(pool)
    .await
}

/// Delivers one queued row: its `user` message saved, and the row marked
/// delivered and linked to it, in one transaction, so a stop between the
/// two can't save it twice. `None` when another drain delivered it first
/// or the recipient is gone.
pub async fn deliver_agent_message(
    pool: &PgPool,
    row: &AgentMessage,
    content: &[ContentBlock],
) -> Result<Option<Message>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_conversations(&mut tx, &[(row.to_conversation_id, RowLock::NoKeyUpdate)]).await? {
        return Ok(None);
    }
    let marked = sqlx::query("UPDATE agent_messages SET delivered_at = now() WHERE id = $1 AND delivered_at IS NULL")
        .bind(row.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if marked == 0 {
        return Ok(None);
    }
    let saved = super::conversations::create_message_on(&mut tx, row.to_conversation_id, "user", content).await?;
    sqlx::query("UPDATE agent_messages SET delivered_message_id = $2 WHERE id = $1")
        .bind(row.id)
        .bind(saved.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(saved))
}
