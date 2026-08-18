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

const DEFAULT_TITLE: &str = "New Conversation";

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
    let content_json = serde_json::to_string(content)
        .expect("ContentBlock always serializes: no non-string map keys, no floats");

    let message = sqlx::query_as::<_, Message>(
        "INSERT INTO messages (conversation_id, role, content) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(conversation_id)
    .bind(role)
    .bind(&content_json)
    .fetch_one(pool)
    .await?;

    // Bump updated_at so the sidebar can sort by recency; auto-title the
    // conversation from the first user message if it's still the default.
    sqlx::query(
        "UPDATE conversations
         SET updated_at = now(),
             title = CASE
                 WHEN title = $1 AND $2 = 'user' THEN left($3, 60)
                 ELSE title
             END
         WHERE id = $4",
    )
    .bind(DEFAULT_TITLE)
    .bind(role)
    .bind(title_candidate(content))
    .bind(conversation_id)
    .execute(pool)
    .await?;

    Ok(message)
}

pub async fn delete_conversation(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// --- Terminal (pod/terminal/command lifecycle) ---
// Server-only, no client/server boundary to cross (no UI yet) — unlike
// Conversation/Message, these don't need to live in models.rs or derive
// anything WASM-relevant. See docs/projects/plans/sandbox-terminal.md.

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

pub async fn create_sandbox_pod(pool: &PgPool, conversation_id: i64) -> Result<SandboxPod, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>("INSERT INTO sandbox_pods (conversation_id) VALUES ($1) RETURNING *")
        .bind(conversation_id)
        .fetch_one(pool)
        .await
}

/// Idempotent by design (unlike creation) — terminating an already-
/// terminated pod converges on the same end state, so it's safe to call
/// without first checking. A `pod_id` that doesn't exist at all is a
/// separate, real error, surfaced by the caller checking the row count
/// via `RETURNING` returning nothing — see `sandbox.rs`.
pub async fn terminate_sandbox_pod(pool: &PgPool, pod_id: i64) -> Result<Option<SandboxPod>, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "UPDATE sandbox_pods SET terminated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(pod_id)
    .fetch_optional(pool)
    .await
}

/// Live pods only (`terminated_at IS NULL`) — a terminated pod's row
/// sticks around (see the plan's "How") but shouldn't be listed as if it
/// still existed.
pub async fn list_sandbox_pods(pool: &PgPool, conversation_id: i64) -> Result<Vec<SandboxPod>, sqlx::Error> {
    sqlx::query_as::<_, SandboxPod>(
        "SELECT * FROM sandbox_pods WHERE conversation_id = $1 AND terminated_at IS NULL ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

pub async fn create_sandbox_terminal(pool: &PgPool, pod_id: i64) -> Result<SandboxTerminal, sqlx::Error> {
    sqlx::query_as::<_, SandboxTerminal>("INSERT INTO sandbox_terminals (pod_id) VALUES ($1) RETURNING *")
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
pub async fn sandbox_terminal_pod_id(pool: &PgPool, terminal_id: i64) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT pod_id FROM sandbox_terminals WHERE id = $1")
        .bind(terminal_id)
        .fetch_optional(pool)
        .await
}

/// Resolves which conversation's `events::publish` bus a pod-scoped call
/// (`terminate_pod`, `create_terminal`, `terminate_terminal`, crash
/// cleanup) should target — see
/// `docs/projects/completed/20260815-sandbox-visibility.md`.
pub async fn sandbox_pod_conversation_id(pool: &PgPool, pod_id: i64) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT conversation_id FROM sandbox_pods WHERE id = $1")
        .bind(pod_id)
        .fetch_optional(pool)
        .await
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
    /// stream's tail independently, see `api::chat::fetch_command_summary`)
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
/// per conversation — see the plan's "What"), enforced at the tool layer,
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
    sqlx::query("INSERT INTO terminal_events (command_id, stream, seq, data) VALUES ($1, $2, $3, $4)")
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
/// `exit_code` stays `NULL` — see the plan's "Agent crash recovery."
/// Restricted to rows still `running` so this is safe to call defensively
/// without first checking status.
pub async fn mark_terminal_command_lost(pool: &PgPool, command_id: &str) -> Result<(), sqlx::Error> {
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
/// assigned `seq` — *not* `id` (see the plan's "Ordering" section on why
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

pub async fn mark_terminal_command_notified(pool: &PgPool, command_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE terminal_commands SET notified_at = now() WHERE command_id = $1")
        .bind(command_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Backs `list_commands` — most-recent-first, bounded the same way
/// `read_terminal_output` is, scoped to one terminal (its history
/// outlives that terminal being torn down, since `terminal_commands`
/// isn't cascade-deleted by `terminate_terminal` — see the plan's "How").
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

// --- MCP servers (externally-hosted, configured via the /mcp-servers UI) ---
// Plain CRUD, no soft delete — this is configuration a person edits, not a
// live external resource like a sandbox pod. See
// docs/projects/completed/20260817-mcp-servers.md.

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct McpServerConfig {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub extra_headers: sqlx::types::Json<HashMap<String, String>>,
    /// `"static_headers"` or `"oauth"` — see
    /// docs/projects/plans/mcp-oauth.md. A plain `String`, not a Rust enum:
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
    /// confirmed live (see docs/projects/plans/mcp-oauth.md). Set only at
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

pub async fn get_mcp_server_config(pool: &PgPool, id: i64) -> Result<Option<McpServerConfig>, sqlx::Error> {
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
/// docs/projects/plans/mcp-oauth.md's "Data model": a grant is bound to the
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

#[cfg(test)]
mod tests {
    use super::*;

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
        create_conversation(pool).await.expect("create conversation")
    }

    /// A pod + terminal pair, for tests that only care about
    /// `terminal_commands`/`terminal_events` and just need a valid
    /// `terminal_id` to hang them off — most of this module.
    async fn test_terminal(pool: &PgPool, conversation_id: i64) -> i64 {
        let pod = create_sandbox_pod(pool, conversation_id).await.expect("create sandbox pod");
        let terminal = create_sandbox_terminal(pool, pod.id).await.expect("create sandbox terminal");
        terminal.id
    }

    #[sqlx::test]
    async fn test_create_sandbox_pod_and_terminate_is_idempotent(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id).await.expect("create pod");
        assert!(pod.terminated_at.is_none());

        let listed = list_sandbox_pods(&pool, conversation.id).await.expect("list pods");
        assert!(listed.iter().any(|p| p.id == pod.id));

        let terminated = terminate_sandbox_pod(&pool, pod.id)
            .await
            .expect("terminate should succeed")
            .expect("pod should exist");
        assert!(terminated.terminated_at.is_some());

        let listed_after = list_sandbox_pods(&pool, conversation.id).await.expect("list pods");
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
        let pod = create_sandbox_pod(&pool, conversation.id).await.expect("create pod");
        let terminal = create_sandbox_terminal(&pool, pod.id).await.expect("create terminal");

        assert_eq!(
            sandbox_terminal_pod_id(&pool, terminal.id).await.expect("lookup"),
            Some(pod.id)
        );

        let for_pod = list_sandbox_terminals_for_pod(&pool, pod.id).await.expect("list for pod");
        assert!(for_pod.iter().any(|t| t.id == terminal.id));
        let for_conversation = list_sandbox_terminals_for_conversation(&pool, conversation.id)
            .await
            .expect("list for conversation");
        assert!(for_conversation.iter().any(|t| t.id == terminal.id));

        terminate_sandbox_terminal(&pool, terminal.id)
            .await
            .expect("terminate should succeed")
            .expect("terminal should exist");

        let for_pod_after = list_sandbox_terminals_for_pod(&pool, pod.id).await.expect("list for pod");
        assert!(
            !for_pod_after.iter().any(|t| t.id == terminal.id),
            "terminated terminal should no longer be listed as live"
        );
    }

    #[sqlx::test]
    async fn test_sandbox_pod_conversation_id_resolves_and_returns_none_for_unknown(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id).await.expect("create pod");

        assert_eq!(
            sandbox_pod_conversation_id(&pool, pod.id).await.expect("lookup"),
            Some(conversation.id)
        );
        assert_eq!(
            sandbox_pod_conversation_id(&pool, pod.id + 999_999).await.expect("lookup"),
            None
        );
    }

    #[sqlx::test]
    async fn test_terminating_a_pod_cascades_to_its_terminals(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id).await.expect("create pod");
        let terminal = create_sandbox_terminal(&pool, pod.id).await.expect("create terminal");

        terminate_sandbox_pod(&pool, pod.id).await.expect("terminate").expect("pod exists");

        // Hard DB cascade only fires on conversation deletion (see the
        // plan's "How") — terminating the pod itself is a soft delete and
        // does *not* cascade to its terminals; the model is expected to
        // terminate_terminal each one first (enforced at the tool layer,
        // not here). Confirm the terminal row is untouched by the pod's
        // own soft delete.
        let still_there = sandbox_terminal_pod_id(&pool, terminal.id).await.expect("lookup");
        assert_eq!(still_there, Some(pod.id), "terminating a pod should not itself touch its terminal rows");
    }

    #[sqlx::test]
    async fn test_create_terminal_command_starts_running(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-1", "echo hi")
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

        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-2", "sleep 5")
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
            terminal_command_is_running(&pool, terminal_a).await.expect("query").is_some(),
            "terminal_a has a running command"
        );
        assert!(
            terminal_command_is_running(&pool, terminal_b).await.expect("query").is_none(),
            "terminal_b should be unaffected by terminal_a's running command"
        );
    }

    #[sqlx::test]
    async fn test_mark_terminal_command_finished_sets_status_and_exit_code(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-3", "false")
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
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-4", "sleep 100")
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
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-5", "sleep 100")
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
            stdout_only.iter().map(|l| l.data.as_str()).collect::<Vec<_>>(),
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
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-7", "seq 5")
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

    #[sqlx::test]
    async fn test_unnotified_finished_terminal_commands_matches_finished_and_lost_only(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let _running = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-9", "sleep 1")
            .await
            .expect("create");
        let finished = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-10", "true")
            .await
            .expect("create");
        mark_terminal_command_finished(&pool, &finished.command_id, 0)
            .await
            .expect("mark finished");
        let lost = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-11", "sleep 100")
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
        assert!(!ids.contains(&"cmd-9"), "still-running command should not need notification");

        mark_terminal_command_notified(&pool, &finished.command_id)
            .await
            .expect("mark notified");
        let remaining = unnotified_finished_terminal_commands(&pool, conversation.id)
            .await
            .expect("query should succeed");
        let ids: Vec<_> = remaining.iter().map(|c| c.command_id.as_str()).collect();
        assert!(!ids.contains(&"cmd-10"), "notified command should drop out");
        assert!(ids.contains(&"cmd-11"), "still-unnotified command should remain");
    }

    #[sqlx::test]
    async fn test_list_terminal_commands_is_most_recent_first_and_bounded(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        for i in 1..=5 {
            create_terminal_command(&pool, conversation.id, terminal_id, &format!("list-cmd-{i}"), "echo")
                .await
                .expect("create");
        }

        let limited = list_terminal_commands(&pool, terminal_id, 3)
            .await
            .expect("list should succeed");
        assert_eq!(limited.len(), 3);
        assert_eq!(
            limited.iter().map(|c| c.command_id.as_str()).collect::<Vec<_>>(),
            vec!["list-cmd-5", "list-cmd-4", "list-cmd-3"]
        );
    }

    #[sqlx::test]
    async fn test_delete_conversation_cascades_to_terminal_commands_and_events(pool: PgPool) {
        let conversation = test_conversation(&pool).await;
        let terminal_id = test_terminal(&pool, conversation.id).await;
        let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-cascade", "echo hi")
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
        // DELETEing (see the plan's "How"): list_commands should still be
        // able to show what ran in a terminal that's since been torn down.
        let conversation = test_conversation(&pool).await;
        let pod = create_sandbox_pod(&pool, conversation.id).await.expect("create pod");
        let terminal = create_sandbox_terminal(&pool, pod.id).await.expect("create terminal");
        let command = create_terminal_command(&pool, conversation.id, terminal.id, "cmd-survives", "echo hi")
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
        assert!(status.is_some(), "command history should survive terminate_terminal");

        let history = list_terminal_commands(&pool, terminal.id, 10)
            .await
            .expect("list should still work against a terminated terminal");
        assert!(history.iter().any(|c| c.command_id == command.command_id));
    }

    #[sqlx::test]
    async fn test_create_list_get_delete_mcp_server_config_round_trip(pool: PgPool) {
        let headers = HashMap::from([("Authorization".to_string(), "Bearer secret".to_string())]);
        let created = create_mcp_server_config(&pool, "github", "https://api.githubcopilot.com/mcp/", &headers, "static_headers", None, None)
            .await
            .expect("create mcp server config");
        assert_eq!(created.name, "github");
        assert_eq!(created.extra_headers.0, headers);

        let all = list_mcp_server_configs(&pool).await.expect("list mcp server configs");
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
        let original_headers =
            HashMap::from([("Authorization".to_string(), "Bearer original-token".to_string())]);
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &original_headers, "static_headers", None, None)
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
        let created = create_mcp_server_config(&pool, "github", "https://api.githubcopilot.com/mcp/", &headers, "static_headers", None, None)
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
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &original, "static_headers", None, None)
            .await
            .expect("create mcp server config");

        let upsert = HashMap::from([
            ("Authorization".to_string(), "Bearer new".to_string()),
            ("X-New".to_string(), "brand-new".to_string()),
        ]);
        let updated = update_mcp_server_config(&pool, created.id, "github", "https://example.com/mcp", &upsert, &[], "static_headers")
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
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &original, "static_headers", None, None)
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
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &original, "static_headers", None, None)
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
        let result =
            update_mcp_server_config(&pool, 999_999, "name", "https://example.com", &HashMap::new(), &[], "static_headers")
                .await
                .expect("query should succeed");
        assert!(result.is_none());
    }

    #[sqlx::test]
    async fn test_create_mcp_server_config_defaults_auth_mode_to_static_headers(pool: PgPool) {
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &HashMap::new(), "static_headers", None, None)
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
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &HashMap::new(), "oauth", None, None)
            .await
            .expect("create mcp server config");
        assert_eq!(created.auth_mode, "oauth");
    }

    #[sqlx::test]
    async fn test_set_mcp_server_oauth_credentials_round_trips(pool: PgPool) {
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &HashMap::new(), "oauth", None, None)
            .await
            .expect("create mcp server config");

        let credentials = serde_json::json!({"client_id": "abc", "token_response": null, "granted_scopes": []});
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
        assert!(cleared.oauth_credentials.is_none(), "None should clear the stored credentials");
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_clears_oauth_credentials_when_url_changes(pool: PgPool) {
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &HashMap::new(), "oauth", None, None)
            .await
            .expect("create mcp server config");
        set_mcp_server_oauth_credentials(&pool, created.id, Some(serde_json::json!({"client_id": "abc"})))
            .await
            .expect("set oauth credentials");

        // Changing the URL invalidates a grant that was issued for the old
        // audience — see docs/projects/plans/mcp-oauth.md's "Data model."
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

        assert!(updated.oauth_credentials.is_none(), "a URL change must clear stored OAuth credentials");
    }

    #[sqlx::test]
    async fn test_update_mcp_server_config_keeps_oauth_credentials_when_url_is_unchanged(pool: PgPool) {
        let created = create_mcp_server_config(&pool, "github", "https://example.com/mcp", &HashMap::new(), "oauth", None, None)
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
}
