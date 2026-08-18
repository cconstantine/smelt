use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "server")]
use crate::db;

/// Read-only summary of a configured MCP server for the browser — never
/// carries header *values*, only their names (see
/// `docs/projects/completed/20260817-mcp-servers.md`). Defined outside
/// any server-gated module since this type itself crosses
/// the client/server boundary as a server-function return value — the same
/// placement `anthropic::tools::TaskSummary` already uses for the same
/// reason.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpServerSummary {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub header_names: Vec<String>,
    /// `"static_headers"` or `"oauth"` — see
    /// docs/projects/plans/mcp-oauth.md. Drives which controls
    /// `/mcp-servers`' edit page shows (header editor vs. Connect/
    /// Reconnect/Disconnect).
    pub auth_mode: String,
    /// A pre-registered OAuth client's id — not secret (visible in the
    /// authorization URL regardless), so shown back plain. `None` means
    /// "use Dynamic Client Registration" (most MCP-spec-compliant
    /// servers). See `McpServerConfig::oauth_client_id`.
    pub oauth_client_id: Option<String>,
    /// Whether a client *secret* is configured — never the value itself,
    /// same write-only precedent as header values.
    pub has_oauth_client_secret: bool,
}

#[cfg(feature = "server")]
impl From<db::McpServerConfig> for McpServerSummary {
    fn from(config: db::McpServerConfig) -> Self {
        let mut header_names: Vec<String> = config.extra_headers.0.into_keys().collect();
        header_names.sort();
        Self {
            id: config.id,
            name: config.name,
            url: config.url,
            header_names,
            auth_mode: config.auth_mode,
            oauth_client_id: config.oauth_client_id,
            has_oauth_client_secret: config.oauth_client_secret.is_some(),
        }
    }
}

/// Whether a server's tools are actually reachable right now — driven by a
/// real connection attempt (`crate::mcp::connection_check`), not a cached
/// guess. See `/mcp-servers`' index badge and its edit page's full status.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum McpConnectionStatus {
    Connected { tool_names: Vec<String> },
    Unreachable { error: String },
    /// `auth_mode == "oauth"` and no OAuth flow has ever completed for this
    /// server (or it was disconnected) — distinct from `Unreachable`
    /// because there's no network problem to report, just nothing to
    /// connect with yet.
    NotConnected,
}

#[get("/api/mcp-servers")]
pub async fn list_mcp_servers() -> ServerFnResult<Vec<McpServerSummary>> {
    let configs = db::list_mcp_server_configs(db::get())
        .await
        .map_err(ServerFnError::new)?;
    Ok(configs.into_iter().map(McpServerSummary::from).collect())
}

#[get("/api/mcp-servers/{id}")]
pub async fn get_mcp_server(id: i64) -> ServerFnResult<McpServerSummary> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    Ok(config.into())
}

/// Attempts a real connection to `id`'s server right now — see
/// `crate::mcp::connection_check`'s doc comment for why this isn't cached.
#[get("/api/mcp-servers/{id}/status")]
pub async fn mcp_server_status(id: i64) -> ServerFnResult<McpConnectionStatus> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;

    if config.auth_mode == "oauth" && config.oauth_credentials.is_none() {
        return Ok(McpConnectionStatus::NotConnected);
    }

    Ok(match crate::mcp::connection_check(db::get(), &config).await {
        Ok(tool_names) => McpConnectionStatus::Connected { tool_names },
        Err(error) => McpConnectionStatus::Unreachable { error },
    })
}

#[post("/api/mcp-servers")]
pub async fn create_mcp_server(
    name: String,
    url: String,
    extra_headers: HashMap<String, String>,
    auth_mode: String,
    oauth_client_id: Option<String>,
    oauth_client_secret: Option<String>,
) -> ServerFnResult<McpServerSummary> {
    let config = db::create_mcp_server_config(
        db::get(),
        &name,
        &url,
        &extra_headers,
        &auth_mode,
        oauth_client_id.as_deref(),
        oauth_client_secret.as_deref(),
    )
    .await
    .map_err(ServerFnError::new)?;
    Ok(config.into())
}

/// The single save action for the `/mcp-servers/{id}` edit page's one
/// form: sets name/URL outright and merges `upsert`/`remove` into the
/// existing headers (see `db::update_mcp_server_config`'s doc comment for
/// the exact merge semantics) — the browser never needs to know an
/// existing header's real value to keep it, change it, or drop it.
///
/// Always evicts `crate::mcp`'s cached connection for this row afterward —
/// a stale connection pointed at the old URL/headers must never survive an
/// edit.
#[post("/api/mcp-servers/{id}")]
pub async fn update_mcp_server(
    id: i64,
    name: String,
    url: String,
    upsert_headers: HashMap<String, String>,
    remove_headers: Vec<String>,
    auth_mode: String,
) -> ServerFnResult<McpServerSummary> {
    let config = db::update_mcp_server_config(db::get(), id, &name, &url, &upsert_headers, &remove_headers, &auth_mode)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    crate::mcp::evict(id).await;
    Ok(config.into())
}

#[delete("/api/mcp-servers/{id}")]
pub async fn delete_mcp_server(id: i64) -> ServerFnResult<()> {
    db::delete_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?;
    crate::mcp::evict(id).await;
    Ok(())
}

/// Starts an OAuth authorization attempt for server `id` and returns the
/// URL the browser must navigate to (a real full-page navigation to the
/// authorization provider, not something this call itself redirects to —
/// see `docs/projects/plans/mcp-oauth.md`'s "UI flow"). The redirect_uri
/// smelt registers is built from *this request's own* Host header (plus
/// `X-Forwarded-Proto`, since a bare `Host` never carries scheme) — see the
/// plan's "Answered by the user": derived, not a configured base URL.
#[post("/api/mcp-servers/{id}/oauth/start")]
pub async fn start_mcp_server_oauth(id: i64) -> ServerFnResult<String> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    let redirect_uri = format!("{}/oauth/mcp-callback/{id}", crate::mcp_oauth::request_base_url().await?);
    crate::mcp_oauth::start(db::get(), &config, redirect_uri).await.map_err(ServerFnError::new)
}

/// Clears server `id`'s stored OAuth credentials without deleting the
/// server row — the edit page's "Disconnect" button.
#[post("/api/mcp-servers/{id}/oauth/disconnect")]
pub async fn disconnect_mcp_server_oauth(id: i64) -> ServerFnResult<()> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    crate::mcp_oauth::disconnect(db::get(), &config).await.map_err(ServerFnError::new)?;
    crate::mcp::evict(id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_server_summary_exposes_header_names_but_not_values() {
        let config = db::McpServerConfig {
            id: 1,
            name: "github".to_string(),
            url: "https://api.githubcopilot.com/mcp/".to_string(),
            extra_headers: sqlx::types::Json(HashMap::from([
                ("Authorization".to_string(), "Bearer super-secret-token".to_string()),
                ("X-Api-Key".to_string(), "another-secret".to_string()),
            ])),
            auth_mode: "static_headers".to_string(),
            oauth_credentials: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            created_at: Default::default(),
            updated_at: Default::default(),
        };

        let summary = McpServerSummary::from(config);

        assert_eq!(summary.header_names, vec!["Authorization", "X-Api-Key"]);
        // The summary type structurally cannot carry header values (it has
        // no such field) — this assertion is the behavioral half of that
        // guarantee: neither secret string leaks into any field we do have.
        assert_ne!(summary.name, "Bearer super-secret-token");
        assert_ne!(summary.url, "another-secret");
    }
}
