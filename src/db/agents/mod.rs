//! Child agents' rows (SME-236): a child's `agents` row and the
//! `agent_messages` queue that carries reports and notices to a
//! conversation. The lifecycle around them is `crate::agents`.
//!
//! Lock order, in every transaction here: the remote's trust lock, if any;
//! then `conversations` rows in ascending id (`lock_conversations`); then
//! everything else. A parent's id is always smaller than its children's.

use super::*;

mod delete;
mod messages;
mod start;
mod tree;
mod volumes;

pub use delete::*;
pub use messages::*;
pub use start::*;
pub use tree::*;
pub use volumes::*;

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
/// `agent_messages.kind`: a parent's or a child's `message_agent` (SME-238).
pub const KIND_MESSAGE: &str = "message";
/// `agent_messages.kind`: a child's `ask_parent`, queued to its parent.
pub const KIND_QUESTION: &str = "question";
/// `agent_messages.kind`: a `Notice::Deliver` waiting for its wake, its
/// body the notice as said, already marked (SME-450).
pub const KIND_SERVER_NOTICE: &str = "server_notice";

/// One queued report or notice, as the drain reads it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AgentMessage {
    pub id: i64,
    pub to_conversation_id: i64,
    /// The sender, `None` for smelt's own notices and once it's deleted.
    pub from_conversation_id: Option<i64>,
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

/// The row's `credentials` column (SME-242, D3), `git-ssh` only: it is
/// Rule G's chain bit — a named secret's name never enters this column,
/// so a descendant's set can never narrow by an ancestor's list.
/// Outer `None` is no `agents` row (a Rule G root); inner `None` is the
/// NULL a pre-ticket row carries, which every Rule G read takes as
/// granted rather than stranding a conversation smelt wrote.
pub async fn agent_credentials(pool: &PgPool, id: i64) -> Result<Option<Option<Vec<String>>>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<Vec<String>>>("SELECT credentials FROM agents WHERE conversation_id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Narrow a starting child's chain bit (SME-242, D3's write 2): the
/// column ∩ what the persona's own `credentials` lists — and since
/// `git-ssh` is the column's only member, the intersection turns on
/// whether the persona listed it. `wants_git_ssh` false, or a column
/// already empty, gives `'{}'` (keeping, never granting); a NULL
/// pre-ticket column reads granted the way every Rule G read does, so
/// a persona that wants the key gets the bit written rather than the
/// row stranded. Gated on `starting` like every write here: a child
/// stopped mid-start is `end_start`'s clear to settle, not this.
/// Says whether it wrote.
pub async fn narrow_agent_credentials(pool: &PgPool, child: i64, wants_git_ssh: bool) -> Result<bool, sqlx::Error> {
    let wrote = sqlx::query(
        "UPDATE agents SET credentials =
             CASE WHEN $2 AND COALESCE($3 = ANY(credentials), TRUE)
                  THEN ARRAY[$3]::text[] ELSE '{}'::text[] END
         WHERE conversation_id = $1 AND state = $4",
    )
    .bind(child)
    .bind(wants_git_ssh)
    .bind(crate::credentials::CREDENTIAL_GIT_SSH)
    .bind(AGENT_STARTING)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(wrote == 1)
}

/// Empty a never-completed child's column (SME-242, D3's write 3):
/// `end_start` calls this on every way out that didn't complete,
/// before it reconciles the pod, so a failed or cut-off start leaves
/// no resumable credential behind (Trade-off 4). The gate is the
/// start's incompletion rather than a state — `started_at IS NULL` —
/// and it has to be, because every way out has already moved the
/// state by the time the exit runs: `fail_start` has made the row
/// `failed`, a user's Stop `stopped`, and those rows are exactly the
/// never-started ones to clear. `started_at` is written only by the
/// start's done step, so once it is set no write here can reach a
/// live conversation; a child stopped while starting keeps it null and
/// can never become `active` (see `database.md`), so clearing it
/// strands nothing. Says whether it wrote.
pub async fn clear_agent_credentials(pool: &PgPool, child: i64) -> Result<bool, sqlx::Error> {
    let wrote = sqlx::query("UPDATE agents SET credentials = '{}' WHERE conversation_id = $1 AND started_at IS NULL")
        .bind(child)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(wrote == 1)
}

/// The credential names this conversation's persona snapshot lists
/// (SME-242): the `credentials` key of `persona_settings`, written once
/// at `complete_start` — the names are fixed for the conversation's
/// life, which of them the store backs is read fresh. No row, no
/// snapshot or no key all mean none. Whether each name is a stored
/// secret is Rule S's store resolution, not this read.
pub async fn persona_credential_names(pool: &PgPool, id: i64) -> Result<Vec<String>, sqlx::Error> {
    let settings =
        sqlx::query_scalar::<_, Option<serde_json::Value>>("SELECT persona_settings FROM agents WHERE conversation_id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(settings
        .flatten()
        .as_ref()
        .and_then(|s| s.get("credentials"))
        .and_then(|c| c.as_array())
        .map(|list| list.iter().filter_map(|name| name.as_str().map(String::from)).collect())
        .unwrap_or_default())
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

/// Whether conversation `id` still exists, read in `conn`'s transaction.
async fn conversation_exists_on(conn: &mut sqlx::PgConnection, id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM conversations WHERE id = $1)")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
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

/// The agent's stored `rule_chain` (SME-241): the JSON array as written,
/// `None` for no agent row or a NULL chain — both mean allow-all. One
/// indexed read; the primary key is the conversation id.
pub async fn agent_rule_chain(pool: &PgPool, id: i64) -> Result<Option<serde_json::Value>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT rule_chain FROM agents WHERE conversation_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .flatten())
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
    /// A message to the child is still unread (SME-238): it reads it
    /// first, or it would be `finished` with no way to report on it. A
    /// report from one of its own agents counts too (SME-354 B2).
    MessageWaiting,
    /// Agents of its own are still `starting` or `active` (SME-354 B2):
    /// their ids and states, in ascending id. Their reports would reach
    /// a finished agent, whose turn sends nothing on.
    AgentsRunning(Vec<(i64, String)>),
    /// A `terminal_commands` row of its own still runs (SME-389 B): a
    /// *first* finish is refused, and these name why — command id,
    /// command line, seconds running, in ascending id. Its command
    /// notices would wake a `finished` seat to nothing, and the seat that
    /// finishes mid-work spends its one report on a placeholder.
    CommandsRunning(Vec<(String, String, i64)>),
}

/// B's read (SME-389): the seat's still-running tracked commands, `$1` its
/// conversation. Same predicate as `idle_agent_without_report`'s
/// running-command clause; both are answered from
/// `idx_terminal_commands_conversation_running` — the index the plan
/// believed still existed (review round 1's Low), `terminal_commands`
/// keeping its finished rows.
pub(crate) const FINISH_RUNNING_COMMANDS_SQL: &str =
    "SELECT command_id, command, EXTRACT(EPOCH FROM (now() - created_at))::BIGINT
     FROM terminal_commands WHERE conversation_id = $1 AND status = 'running' ORDER BY id";

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
    // SME-389 B: a first finish with a tracked command still running is
    // refused — the command's finished notice would wake a `finished`
    // seat to nothing, and the seat that finishes mid-work spends its one
    // report on a placeholder (the ticket's incident). Read here, after
    // the state is proven finishable and inside this transaction, so a
    // refusal and any enqueue can't interleave; the update above rolls
    // back with it. Commands a seat detached (`&`, nohup) make no rows
    // and are not gated, as in the pod rules. Same predicate as
    // `idle_agent_without_report`'s running-command clause.
    let running_commands = sqlx::query_as::<_, (String, String, i64)>(FINISH_RUNNING_COMMANDS_SQL)
        .bind(child)
        .fetch_all(&mut *tx)
        .await?;
    if !running_commands.is_empty() {
        return Ok(Finish::CommandsRunning(running_commands));
    }
    // Its own agents still running would report to a finished agent,
    // which sends nothing on (SME-354 B2): it waits for them or stops
    // them first. One of them finishing at the same moment is read
    // here still `active` (not yet committed) or `finished` with its
    // report queued (committed), which the unread check below refuses.
    let running = sqlx::query_as::<_, (i64, String)>(
        "SELECT a.conversation_id, a.state FROM agents a JOIN conversations c ON c.id = a.conversation_id
         WHERE c.parent_id = $1 AND a.state IN ($2, $3) ORDER BY a.conversation_id",
    )
    .bind(child)
    .bind(AGENT_STARTING)
    .bind(AGENT_ACTIVE)
    .fetch_all(&mut *tx)
    .await?;
    if !running.is_empty() {
        return Ok(Finish::AgentsRunning(running));
    }
    // A statement after the update got the row: a `message_agent` that
    // held it first has committed, and its row is seen (SME-238).
    if unread_by(&mut tx, child).await? {
        return Ok(Finish::MessageWaiting);
    }
    queue_agent_message(&mut tx, parent, Some(child), label, KIND_REPORT, report).await?;
    tx.commit().await?;
    #[cfg(test)]
    crate::agents::test_hooks::reach(crate::agents::test_hooks::FINISH_COMMITTED, child).await;
    Ok(Finish::Queued { parent })
}

/// Whether a message, a question or (SME-354 B2) a report from one of
/// its own agents to `child` is still undelivered, or an answer to its
/// own question is still untaken, on the caller's transaction.
async fn unread_by(conn: &mut sqlx::PgConnection, child: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM agent_messages
                        WHERE to_conversation_id = $1 AND delivered_at IS NULL AND kind IN ($2, $3, $4))
             OR EXISTS (SELECT 1 FROM pending_questions WHERE conversation_id = $1 AND answer IS NOT NULL)",
    )
    .bind(child)
    .bind(KIND_MESSAGE)
    .bind(KIND_QUESTION)
    .bind(KIND_REPORT)
    .fetch_one(&mut *conn)
    .await
}

/// `report_after_finish`'s outcome (SME-389 A).
#[derive(Debug, PartialEq)]
pub enum FollowUp {
    /// The follow-up report is queued to this parent.
    Queued { parent: i64 },
    /// The seat isn't `finished` after all (a `message_agent` or a stop
    /// landed while its report was in flight): this state, named.
    StateChanged(String),
    /// The seat, its parent, or either's row is gone.
    Gone,
}

/// A late `finish`'s follow-up (SME-389 A): the report of a seat already
/// `finished` queued to its parent anyway, so no call a seat makes as
/// `finish` disappears quietly (the ticket's 42-minute stall). Locks the
/// same rows in the same order as `finish_agent` — `conversations` rows
/// ascending `KEY SHARE`, then the seat's `agents` row `FOR NO KEY UPDATE`,
/// the lock `message_agent` takes on a finished recipient — so every
/// interleaving resolves by row locks, and writes no state: the seat stays
/// `finished`. B's running-command rule is a first-finish rule only; a
/// follow-up is never gated, the seat has nothing left to spend.
pub async fn report_after_finish(pool: &PgPool, child: i64, label: &str, report: &str) -> Result<FollowUp, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(parent) = sqlx::query_scalar::<_, Option<i64>>("SELECT parent_id FROM conversations WHERE id = $1")
        .bind(child)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
    else {
        return Ok(FollowUp::Gone);
    };
    if !lock_conversations(&mut tx, &[(parent, RowLock::KeyShare), (child, RowLock::KeyShare)]).await? {
        return Ok(FollowUp::Gone);
    }
    // The lock `message_agent` takes on a finished recipient, so one that
    // reactivated the seat mid-flight reads here `active` (not yet
    // committed) and refuses with the state named, exactly as a normal
    // `finish` would.
    let state = sqlx::query_scalar::<_, String>("SELECT state FROM agents WHERE conversation_id = $1 FOR NO KEY UPDATE")
        .bind(child)
        .fetch_optional(&mut *tx)
        .await?;
    match state.as_deref() {
        Some(AGENT_FINISHED) => {}
        Some(other) => return Ok(FollowUp::StateChanged(other.to_string())),
        None => return Ok(FollowUp::Gone),
    }
    // A follow-up is never gated by a running command (the doc above):
    // the seat has already given up its turn, and there is no second
    // report left to spend on a placeholder.
    queue_agent_message(&mut tx, parent, Some(child), label, KIND_REPORT, report).await?;
    tx.commit().await?;
    Ok(FollowUp::Queued { parent })
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

/// Locks `id` and every conversation under it `FOR NO KEY UPDATE`, the
/// mode `create_child` takes on its caller, and returns their ids as
/// `subtree_ids` does (SME-354 B1). A `create_child` by any of them that
/// held its row first has committed by the time the lock is granted, so
/// the subtree is read again under the locks until nothing new appears;
/// one that comes later waits for the caller's commit and sees its
/// writes (`stopped`, `deleting_at`). A new child's id is larger than
/// every row already locked, so the rows are still taken in ascending id.
pub(super) async fn lock_subtree(conn: &mut sqlx::PgConnection, id: i64) -> Result<Vec<i64>, sqlx::Error> {
    let mut locked: Vec<i64> = Vec::new();
    loop {
        let subtree = subtree_ids(&mut *conn, id).await?;
        let new: Vec<(i64, RowLock)> = subtree
            .iter()
            .filter(|row| !locked.contains(row))
            .map(|row| (*row, RowLock::NoKeyUpdate))
            .collect();
        if new.is_empty() {
            return Ok(subtree);
        }
        lock_conversations(&mut *conn, &new).await?;
        locked.extend(new.iter().map(|(row, _)| *row));
    }
}

/// `stop_agent`'s transaction: `id`, one of `caller`'s children, and its
/// whole subtree set `stopped`, the rows locked in ascending id first
/// (`lock_subtree`, so a grandchild being made meanwhile is in it). The
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
    let subtree = lock_subtree(&mut tx, id).await?;
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

/// Queues `text`, a `Notice::Deliver`'s already-marked words, to `to` for
/// its wake to drain verbatim (SME-450): queued, it survives a restart.
pub async fn queue_server_notice(pool: &PgPool, to: i64, text: &str) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    queue_agent_message(&mut conn, to, None, "smelt", KIND_SERVER_NOTICE, text).await
}

/// The "owes a report with nothing running" predicate (SME-455): an
/// `active` agent with a parent and none of a question waiting, a trust
/// card waiting, a terminal command running, or an agent of its own still
/// starting or active. One shared expression, used only by
/// `IDLE_AGENT_WITHOUT_REPORT_SQL` and the tree's `REPORT_OWED_CASE`;
/// both alias `conversations` as `c` and have its `agents` row as `a`.
/// The state words are inline literals mirroring `AGENT_ACTIVE` and
/// `AGENT_STARTING` (the two hosts bind different parameter lists);
/// `test_the_stuck_predicate_quotes_its_state_literals` keeps them in
/// step. SME-389 R1-1: EXPLAIN-test each host query as it is written.
macro_rules! stuck_without_report {
    () => {
        "c.parent_id IS NOT NULL AND a.state = 'active' \
         AND NOT EXISTS (SELECT 1 FROM pending_questions q WHERE q.conversation_id = c.id AND q.answer IS NULL) \
         AND NOT EXISTS (SELECT 1 FROM instruction_requests r WHERE r.conversation_id = c.id) \
         AND NOT EXISTS (SELECT 1 FROM terminal_commands t WHERE t.conversation_id = c.id AND t.status = 'running') \
         AND NOT EXISTS (SELECT 1 FROM agents g JOIN conversations gc ON gc.id = g.conversation_id \
                         WHERE gc.parent_id = c.id AND g.state IN ('starting', 'active'))"
    };
}

/// The idle check's own query (`$1` the conversation id), a constant so
/// the running-command clause is EXPLAIN-tested as written, not
/// paraphrased (SME-389 R1-1), and built from `stuck_without_report` so
/// the tree's mark and this check cannot drift.
pub(crate) const IDLE_AGENT_WITHOUT_REPORT_SQL: &str = concat!(
    "SELECT c.parent_id,\n            (SELECT m.content FROM messages m WHERE m.conversation_id = c.id AND m.role = 'assistant'\n             ORDER BY m.id DESC LIMIT 1)\n     FROM conversations c JOIN agents a ON a.conversation_id = c.id\n     WHERE c.id = $1 AND (",
    stuck_without_report!(),
    ")"
);

/// The tree query's gated mark (SME-455): `agents.report_owed_at` shows
/// only while `stuck_without_report` still holds, so the mark self-hides
/// while the seat runs something or waits on someone and returns with its
/// original age when those go — it never stopped owing. `state = 'active'`
/// inside the predicate keeps it off stopped, failed and finished seats.
/// Aliases as in the tree query: `conversations c LEFT JOIN agents a`.
pub(crate) const REPORT_OWED_CASE: &str = concat!("CASE WHEN (", stuck_without_report!(), ") THEN a.report_owed_at END");

/// Whether `id` is an `active` agent with nothing to wait on: no question
/// to the user unanswered, no AGENTS.md card, no terminal command running
/// and no agent of its own still starting or active. Its parent then, and
/// the first 500 characters of its last reply's text. `None` otherwise.
pub async fn idle_agent_without_report(pool: &PgPool, id: i64) -> Result<Option<(i64, String)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (i64, Option<String>)>(IDLE_AGENT_WITHOUT_REPORT_SQL)
    .bind(id)
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
    /// Its `ask_parent` question is unanswered (SME-238).
    pub waiting_for_parent: bool,
}

/// `parent`'s own children, newest first: never anyone else's.
pub async fn list_child_agents(pool: &PgPool, parent: i64) -> Result<Vec<ChildAgent>, sqlx::Error> {
    sqlx::query_as::<_, ChildAgent>(&format!(
        "SELECT c.id, c.title, a.persona_path, a.state, a.created_at,
                (SELECT max(m.created_at) FROM messages m WHERE m.conversation_id = c.id) AS last_message_at,
                (SELECT count(*) FROM conversations g WHERE g.parent_id = c.id) AS agents_started,
                {} AS waiting_for_user,
                EXISTS (SELECT 1 FROM pending_questions q
                        WHERE q.conversation_id = c.id AND q.answer IS NULL AND q.asked_of = $2) AS waiting_for_parent
         FROM conversations c JOIN agents a ON a.conversation_id = c.id
         WHERE c.parent_id = $1
         ORDER BY c.id DESC",
        super::WAITS_FOR_USER
    ))
    .bind(parent)
    .bind(super::ASKED_OF_PARENT)
    .fetch_all(pool)
    .await
}

/// Every conversation with an undelivered report, notice or message, and
/// every agent with an answer to its question untaken (SME-238), that
/// isn't a `stopped` agent nor paused by the user: the startup sweep wakes
/// them.
pub async fn conversations_with_queued_messages(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT c.id FROM conversations c LEFT JOIN agents a ON a.conversation_id = c.id
         WHERE c.paused_at IS NULL AND (a.state IS NULL OR a.state <> $1)
           AND (EXISTS (SELECT 1 FROM agent_messages m WHERE m.to_conversation_id = c.id AND m.delivered_at IS NULL)
                OR (a.conversation_id IS NOT NULL
                    AND EXISTS (SELECT 1 FROM pending_questions q WHERE q.conversation_id = c.id AND q.answer IS NOT NULL)))
         ORDER BY c.id",
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
        "SELECT id, to_conversation_id, from_conversation_id, from_label, kind, body FROM agent_messages
         WHERE to_conversation_id = $1 AND delivered_at IS NULL ORDER BY id",
    )
    .bind(to)
    .fetch_all(pool)
    .await
}

/// Delivers one queued row: its `user` message saved, and the row marked
/// delivered and linked to it, in one transaction, so a stop between the
/// two can't save it twice. `None` when another drain delivered it first
/// or the recipient is gone. A turn's drain delivers it, so the message
/// is owed the model's answer (SME-450).
pub async fn deliver_agent_message(
    pool: &PgPool,
    row: &AgentMessage,
    content: &[ContentBlock],
) -> Result<Option<Message>, sqlx::Error> {
    deliver_agent_message_owed(pool, row, content, true).await
}

/// `deliver_agent_message` for a row saved where no turn will answer it
/// (a paused conversation's notice, D9): no answer is owed, since a mark
/// with no turn running says a restart dropped one (review 1, L1).
pub async fn deliver_agent_message_unanswered(
    pool: &PgPool,
    row: &AgentMessage,
    content: &[ContentBlock],
) -> Result<Option<Message>, sqlx::Error> {
    deliver_agent_message_owed(pool, row, content, false).await
}

async fn deliver_agent_message_owed(
    pool: &PgPool,
    row: &AgentMessage,
    content: &[ContentBlock],
    owed: bool,
) -> Result<Option<Message>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // The sender too, `KEY SHARE`, before the row (SME-293): the second
    // UPDATE below re-checks every FK on a row this transaction already
    // wrote, `from_conversation_id` included, and that lock taken after
    // the row's would cross a delete of the sender (whose cascade sets
    // `from` NULL on this row) into a deadlock.
    let mut locks = vec![(row.to_conversation_id, RowLock::NoKeyUpdate)];
    if let Some(from) = row.from_conversation_id.filter(|from| *from != row.to_conversation_id) {
        locks.push((from, RowLock::KeyShare));
    }
    if !lock_conversations(&mut tx, &locks).await? && !conversation_exists_on(&mut tx, row.to_conversation_id).await? {
        return Ok(None); // the recipient is gone
    }
    // A sender gone here was deleted while we waited: its cascade set
    // `from` NULL, and the row still arrives under its kept `from_label`.
    let marked = sqlx::query("UPDATE agent_messages SET delivered_at = now() WHERE id = $1 AND delivered_at IS NULL")
        .bind(row.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if marked == 0 {
        return Ok(None);
    }
    #[cfg(test)]
    crate::agents::test_hooks::reach(crate::agents::test_hooks::DELIVER_MARKED, row.to_conversation_id).await;
    let saved = if owed {
        super::owed::create_owed_message_on(&mut tx, row.to_conversation_id, content).await?
    } else {
        super::conversations::create_message_on(&mut tx, row.to_conversation_id, "user", content).await?
    };
    sqlx::query("UPDATE agent_messages SET delivered_message_id = $2 WHERE id = $1")
        .bind(row.id)
        .bind(saved.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(saved))
}
