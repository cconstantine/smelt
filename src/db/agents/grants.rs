//! What a conversation's persona grants it, as the rows record it
//! (SME-241, SME-242): Rule G's chain bit, Rule S's credential names and
//! the tool-rule chain. Moved out of `agents/mod.rs` for SME-481.

use super::*;

/// The row's `credentials` column (SME-242, D3), `git-ssh` only: it is
/// Rule G's chain bit — a named secret's name never enters this column,
/// so a descendant's set can never narrow by an ancestor's list.
/// Outer `None` is no `agents` row (a Rule G root); inner `None` is the
/// NULL a pre-ticket row carries, which every Rule G read takes as
/// granted rather than stranding a conversation smelt wrote. A top-level
/// conversation set to a persona (SME-481) answers from its
/// `top_level_personas` row the same way: only one with no persona is a
/// Rule G root.
pub async fn agent_credentials(pool: &PgPool, id: i64) -> Result<Option<Option<Vec<String>>>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<Vec<String>>>(
        "SELECT credentials FROM agents WHERE conversation_id = $1
         UNION ALL
         SELECT credentials FROM top_level_personas WHERE conversation_id = $1
         LIMIT 1",
    )
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
/// secret is Rule S's store resolution, not this read. A top-level
/// persona's names come from its own `top_level_personas` snapshot
/// (SME-481), and from nowhere else.
pub async fn persona_credential_names(pool: &PgPool, id: i64) -> Result<Vec<String>, sqlx::Error> {
    let settings = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT persona_settings FROM agents WHERE conversation_id = $1
         UNION ALL
         SELECT persona_settings FROM top_level_personas WHERE conversation_id = $1
         LIMIT 1",
    )
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

/// The token ceiling this conversation's persona snapshot sets (SME-493):
/// the `compact_at` key of `persona_settings`, frozen at the start like
/// `credentials`. Each seat's own, from its `agents` row or its
/// `top_level_personas` one: nothing is inherited. No row, no key or a
/// null all mean none.
pub async fn persona_compact_at(pool: &PgPool, id: i64) -> Result<Option<u64>, sqlx::Error> {
    let settings = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT persona_settings FROM agents WHERE conversation_id = $1
         UNION ALL
         SELECT persona_settings FROM top_level_personas WHERE conversation_id = $1
         LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(settings.flatten().as_ref().and_then(|s| s.get("compact_at")).and_then(serde_json::Value::as_u64))
}

/// The agent's stored `rule_chain` (SME-241): the JSON array as written,
/// `None` for no agent row or a NULL chain — both mean allow-all. One
/// indexed read; the primary key is the conversation id. A top-level
/// conversation set to a persona (SME-481) has its chain in
/// `top_level_personas`: the persona's one entry, `own` included.
pub async fn agent_rule_chain(pool: &PgPool, id: i64) -> Result<Option<serde_json::Value>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT rule_chain FROM agents WHERE conversation_id = $1
         UNION ALL
         SELECT rule_chain FROM top_level_personas WHERE conversation_id = $1
         LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .flatten())
}
