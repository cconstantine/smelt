//! `sandbox_mcp_calls`: one row per MCP call a script in a sandbox made
//! (SME-359). Never the call's arguments or result.

use sqlx::PgPool;

/// Records a call as it starts (or, for one refused by a limit, as it is
/// refused: `outcome` is then final). `command_id` is kept only when it is
/// one of this conversation's commands, so a pod can't charge its calls to
/// another conversation's command, and an id the agent got wrong doesn't
/// fail the insert.
#[allow(clippy::too_many_arguments)]
pub async fn insert_sandbox_mcp_call(
    pool: &PgPool,
    conversation_id: i64,
    pod_id: i64,
    command_id: Option<&str>,
    server: &str,
    tool: &str,
    class: Option<&str>,
    outcome: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO sandbox_mcp_calls (conversation_id, pod_id, command_id, server, tool, class, outcome, finished_at)
         VALUES ($1, $2,
                 (SELECT command_id FROM terminal_commands WHERE command_id = $3 AND conversation_id = $1),
                 $4, $5, $6, $7, CASE WHEN $7 = 'running' THEN NULL ELSE now() END)
         RETURNING id",
    )
    .bind(conversation_id)
    .bind(pod_id)
    .bind(command_id)
    .bind(server)
    .bind(tool)
    .bind(class)
    .bind(outcome)
    .fetch_one(pool)
    .await
}

/// Records how a started call ended. A row that is gone (its conversation
/// or pod was deleted mid-call) is no error: there is nothing to record.
pub async fn finish_sandbox_mcp_call(pool: &PgPool, id: i64, class: Option<&str>, outcome: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sandbox_mcp_calls SET class = COALESCE($2, class), outcome = $3, finished_at = now() WHERE id = $1")
        .bind(id)
        .bind(class)
        .bind(outcome)
        .execute(pool)
        .await?;
    Ok(())
}

/// The budget count for a call traced to a command of this conversation
/// (`$1`, the command id): answered from `idx_sandbox_mcp_calls_command`.
pub(crate) const COMMAND_BUDGET_SQL: &str =
    "SELECT count(*) FROM sandbox_mcp_calls WHERE command_id = $1 AND outcome <> 'limited'";

/// The budget count for a call traced to no command (`$1`, the pod id):
/// answered from the partial index on the pod's command-less calls.
pub(crate) const POD_BUDGET_SQL: &str =
    "SELECT count(*) FROM sandbox_mcp_calls WHERE pod_id = $1 AND command_id IS NULL AND outcome <> 'limited'";

/// The calls counted against a call's budget (SME-359): those of its
/// command, when the command is this conversation's (as the insert keeps
/// it), and otherwise those of its pod made outside any command. A call
/// held back by a limit doesn't count.
pub async fn count_sandbox_mcp_calls_against_budget(
    pool: &PgPool,
    conversation_id: i64,
    pod_id: i64,
    command_id: Option<&str>,
) -> Result<i64, sqlx::Error> {
    // Two plain queries, not one CASE: a condition inside a CASE can't use
    // an index, and this runs on every call (review round 2).
    let owned = match command_id {
        Some(command_id) => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM terminal_commands WHERE command_id = $1 AND conversation_id = $2)",
        )
        .bind(command_id)
        .bind(conversation_id)
        .fetch_one(pool)
        .await?
        .then_some(command_id),
        None => None,
    };
    match owned {
        Some(command_id) => sqlx::query_scalar(COMMAND_BUDGET_SQL).bind(command_id).fetch_one(pool).await,
        None => sqlx::query_scalar(POD_BUDGET_SQL).bind(pod_id).fetch_one(pool).await,
    }
}

/// Whether `pod_id` is `conversation_id`'s live pod, and the conversation
/// isn't being deleted: the check a script's call passes first.
pub async fn sandbox_pod_is_live_for(pool: &PgPool, pod_id: i64, conversation_id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT p.terminated_at IS NULL AND c.deleting_at IS NULL
           FROM sandbox_pods p JOIN conversations c ON c.id = p.conversation_id
          WHERE p.id = $1 AND p.conversation_id = $2",
    )
    .bind(pod_id)
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or(false))
}

/// One recorded call, as the tests read it.
#[cfg(test)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SandboxMcpCall {
    pub conversation_id: i64,
    pub pod_id: i64,
    pub command_id: Option<String>,
    pub server: String,
    pub tool: String,
    pub class: Option<String>,
    pub outcome: String,
    pub started_at: chrono::NaiveDateTime,
    pub finished_at: Option<chrono::NaiveDateTime>,
}

/// Every call recorded for `conversation_id`, oldest first.
#[cfg(test)]
pub async fn list_sandbox_mcp_calls(pool: &PgPool, conversation_id: i64) -> Result<Vec<SandboxMcpCall>, sqlx::Error> {
    sqlx::query_as(
        "SELECT conversation_id, pod_id, command_id, server, tool, class, outcome, started_at, finished_at
           FROM sandbox_mcp_calls WHERE conversation_id = $1 ORDER BY id",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// The recorded calls of each of `command_ids`, grouped by tool and
/// outcome, for their summaries (`api::mcp_calls`): `conversation_id`'s
/// alone, since a command id can come from the pod's agent unchecked
/// (SME-386 M4).
pub async fn sandbox_mcp_call_counts(
    pool: &PgPool,
    conversation_id: i64,
    command_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<crate::api::mcp_calls::McpCallCount>>, sqlx::Error> {
    let rows: Vec<(String, String, String, String, bool, i64)> = sqlx::query_as(
        "SELECT command_id, server, tool, outcome,
                outcome = 'running' AND started_at < now() - make_interval(secs => $2) AS stale,
                count(*)
           FROM sandbox_mcp_calls
          WHERE command_id = ANY($1) AND conversation_id = $3
          GROUP BY 1, 2, 3, 4, 5",
    )
    .bind(command_ids)
    .bind(crate::api::mcp_calls::UNKNOWN_AFTER_SECS as f64)
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;
    let mut counts: std::collections::HashMap<String, Vec<crate::api::mcp_calls::McpCallCount>> = Default::default();
    for (command_id, server, tool, outcome, stale, count) in rows {
        counts.entry(command_id).or_default().push(crate::api::mcp_calls::McpCallCount { server, tool, outcome, stale, count });
    }
    Ok(counts)
}
