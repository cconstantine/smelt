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
