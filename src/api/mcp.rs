use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "server")]
use crate::db;

/// Read-only summary of a configured MCP server for the browser — never
/// carries header *values*, only their names (see
/// SME-15). Defined outside
/// any server-gated module since this type itself crosses
/// the client/server boundary as a server-function return value — the same
/// placement `anthropic::tools::TodoItem` uses for the same reason.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpServerSummary {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub header_names: Vec<String>,
    /// `"static_headers"` or `"oauth"` — see
    /// SME-16. Drives which controls
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
    /// Whether this server's existing name is one no `smelt.tools` pattern
    /// can name (SME-241): it holds `/` or `*`, so its tools can only be
    /// ruled on by class. Rows that predate the name rule keep working;
    /// the edit page says so under the name field, and the name can be
    /// changed to one that is nameable.
    pub name_unrulable: bool,
}

#[cfg(feature = "server")]
impl From<db::McpServerConfig> for McpServerSummary {
    fn from(config: db::McpServerConfig) -> Self {
        // Judged before the name moves into the summary below.
        let name_unrulable = crate::mcp::check_server_name(&config.name).is_err();
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
            name_unrulable,
        }
    }
}

/// Whether `url`, an authorization URL built from an MCP server's OAuth
/// metadata, is safe for the browser to navigate to (B4, SME-122): only
/// `http://` or `https://`, in any case. The metadata comes from the MCP
/// server's authorization server, which smelt doesn't trust, and a
/// `javascript:` (or `data:`) `authorization_endpoint` would run in
/// smelt's own origin when Connect navigates to it. A leading space or
/// control character is refused too: a browser strips those before reading
/// the scheme. Checked by `mcp_oauth::start` before handing the URL out,
/// and again by the edit page before it navigates.
pub fn is_navigable_authorization_url(url: &str) -> bool {
    let head: String = url.chars().take("https://".len()).collect();
    let head = head.to_ascii_lowercase();
    head.starts_with("https://") || head.starts_with("http://")
}

/// Whether a server's tools are actually reachable right now — driven by a
/// real connection attempt (`crate::mcp::connection_check`), not a cached
/// guess. See `/mcp-servers`' index badge and its edit page's full status.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum McpConnectionStatus {
    Connected {
        tool_names: Vec<String>,
    },
    Unreachable {
        error: String,
    },
    /// `auth_mode == "oauth"` and no OAuth flow has ever completed for this
    /// server (or it was disconnected) — distinct from `Unreachable`
    /// because there's no network problem to report, just nothing to
    /// connect with yet.
    NotConnected,
    /// An OAuth server whose sign-in can't be refreshed any more (the
    /// provider refused the refresh token, or there isn't one): only a
    /// Reconnect brings it back (SME-113). Its own status rather than
    /// `Unreachable`, since nothing is wrong with the network.
    NeedsReconnect {
        error: String,
    },
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

    Ok(
        match crate::mcp::connection_check(db::get(), &config).await {
            Ok(tool_names) => McpConnectionStatus::Connected { tool_names },
            Err(crate::mcp::CheckError::Unreachable(error)) => {
                McpConnectionStatus::Unreachable { error }
            }
            Err(crate::mcp::CheckError::SignInExpired(error)) => {
                McpConnectionStatus::NeedsReconnect { error }
            }
        },
    )
}

/// What both write paths check before they touch the database: the name,
/// which a `smelt.tools` pattern must be able to name (SME-241), and the
/// headers the `auth_mode` can have. One seam so create and update can't
/// drift apart, and so the refusal reads without a pool. Server-side like
/// the checks it calls: `crate::mcp` is server-only.
#[cfg(feature = "server")]
fn check_server_fields(name: &str, auth_mode: &str, headers: &HashMap<String, String>) -> Result<(), String> {
    crate::mcp::check_server_name(name)?;
    crate::mcp::check_extra_headers(auth_mode, headers)
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
    check_server_fields(&name, &auth_mode, &extra_headers).map_err(ServerFnError::new)?;
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
    check_server_fields(&name, &auth_mode, &upsert_headers).map_err(ServerFnError::new)?;
    let config = db::update_mcp_server_config(
        db::get(),
        id,
        &name,
        &url,
        &upsert_headers,
        &remove_headers,
        &auth_mode,
    )
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
/// see SME-16's "UI flow"). The redirect_uri
/// smelt registers is built from *this request's own* Host header (plus
/// `X-Forwarded-Proto`, since a bare `Host` never carries scheme) — see the
/// plan's "Answered by the user": derived, not a configured base URL.
#[post("/api/mcp-servers/{id}/oauth/start")]
pub async fn start_mcp_server_oauth(id: i64) -> ServerFnResult<String> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    let redirect_uri = format!(
        "{}/oauth/mcp-callback/{id}",
        crate::mcp_oauth::request_base_url().await?
    );
    crate::mcp_oauth::start(db::get(), &config, redirect_uri)
        .await
        .map_err(ServerFnError::new)
}

/// Server `id`'s last failed OAuth callback's error, if one is kept and
/// unread (SME-168): the edit page calls it once when the callback's
/// redirect brought it there with `?oauth_callback=1`. A `post`, since it
/// removes what it reads (`mcp_oauth::take_callback_error`).
#[post("/api/mcp-servers/{id}/oauth/callback-error")]
pub async fn take_mcp_server_oauth_error(id: i64) -> ServerFnResult<Option<String>> {
    Ok(crate::mcp_oauth::take_callback_error(id))
}

/// Clears server `id`'s stored OAuth credentials without deleting the
/// server row — the edit page's "Disconnect" button.
#[post("/api/mcp-servers/{id}/oauth/disconnect")]
pub async fn disconnect_mcp_server_oauth(id: i64) -> ServerFnResult<()> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    crate::mcp_oauth::disconnect(db::get(), &config)
        .await
        .map_err(ServerFnError::new)?;
    crate::mcp::evict(id).await;
    Ok(())
}

// --- Per-tool class overrides (SME-240) ---

/// One tool's class row for the edit page: the server's own answer from
/// its annotations (`server_class`, `None` when the tool isn't in the
/// current live list — an override saved for a tool that has since
/// disappeared), the user's override (`override_class`, `None` when
/// there is none), and whether the server lists the tool at all
/// (`listed`, `None` when the server was unreachable so the live list
/// is unknown). Classes ride as the `as_str()` texts; the page and the
/// POST both speak those names.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpToolClassRow {
    pub tool_name: String,
    pub server_class: Option<String>,
    /// Whether the server annotated the tool it lists (SME-240: the page
    /// says "from the server" or "the server doesn't annotate it (the
    /// default)" by it). `None` when there's no live listing to read
    /// hints from — the tool isn't listed, or the server is unreachable.
    /// (The plan's struct named this only in `McpToolClassInfo`; the page
    /// can't tell annotation from default without it crossing, so the
    /// merge carries it across.)
    pub annotated: Option<bool>,
    pub override_class: Option<String>,
    pub listed: Option<bool>,
}

/// The edit page's Tool-classes section: one row per listed tool plus
/// one per saved override for a no-longer-listed tool, and `server_note`
/// — why the live list is missing (unreachable, sign-in expired, OAuth
/// not connected yet) — when it is.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct McpToolClasses {
    pub rows: Vec<McpToolClassRow>,
    pub server_note: Option<String>,
}

/// The page's rows: listed tools in the server's own order, each carrying
/// the server's annotation class and (when one exists) its override;
/// then, by tool name, the saved overrides whose tool the current live
/// list no longer has — the design keeps those, and the page offers them
/// for clearing. Pure, so it's testable without a connection.
#[cfg(feature = "server")]
fn merge_tool_class_rows(
    listed: Vec<crate::mcp::McpToolClassInfo>,
    saved: Vec<(String, String)>,
) -> Vec<McpToolClassRow> {
    let overrides: HashMap<&str, &str> =
        saved.iter().map(|(name, class)| (name.as_str(), class.as_str())).collect();
    let mut rows: Vec<McpToolClassRow> = listed
        .iter()
        .map(|tool| McpToolClassRow {
            tool_name: tool.name.clone(),
            server_class: Some(tool.server_class.as_str().to_string()),
            annotated: Some(tool.annotated),
            override_class: overrides.get(tool.name.as_str()).map(|class| (*class).to_string()),
            listed: Some(true),
        })
        .collect();
    let listed_names: std::collections::HashSet<&str> =
        listed.iter().map(|tool| tool.name.as_str()).collect();
    // `db::list_mcp_tool_classes` already delivers by tool name, so the
    // stale overrides come out by name without another sort.
    rows.extend(saved.into_iter().filter(|(name, _)| !listed_names.contains(name.as_str())).map(|(tool_name, class)| McpToolClassRow {
        tool_name,
        server_class: None,
        annotated: None,
        override_class: Some(class),
        listed: Some(false),
    }));
    rows
}

/// The GET's rows when there is no live list to merge with: the saved
/// overrides alone, `listed: None` — the live state is unknown, not
/// absent.
#[cfg(feature = "server")]
fn saved_only_rows(saved: Vec<(String, String)>) -> Vec<McpToolClassRow> {
    saved
        .into_iter()
        .map(|(tool_name, class)| McpToolClassRow {
            tool_name,
            server_class: None,
            annotated: None,
            override_class: Some(class),
            listed: None,
        })
        .collect()
}

/// The POST's tool-name check. The page only ever offers listed and
/// saved names, so a blank one means a hand-built request — refuse it
/// rather than store an unaddressable row.
#[cfg(feature = "server")]
fn check_override_tool_name(tool_name: &str) -> Result<(), String> {
    if tool_name.trim().is_empty() {
        Err("a tool name is required".to_string())
    } else {
        Ok(())
    }
}

/// The POST's class check: the three names, or the table's CHECK's words
/// back to the sender.
#[cfg(feature = "server")]
fn parse_override_class(class: Option<String>) -> Result<Option<crate::anthropic::tools::ToolClass>, String> {
    match class.as_deref() {
        None => Ok(None),
        Some(text) => crate::anthropic::tools::ToolClass::parse(text)
            .map(Some)
            .ok_or_else(|| format!("unknown tool class {text:?}: expected read-only, writing or destructive")),
    }
}

/// The edit page's Tool-classes section: every live tool with the class
/// the server's own annotations give it, merged with the user's saved
/// overrides. When the server can't be listed right now — unreachable,
/// expired sign-in, or an OAuth server never connected — the section is
/// the saved overrides plus a `server_note` saying why, still editable
/// so an override can be cleared while the server is down.
#[get("/api/mcp-servers/{id}/tool-classes")]
pub async fn get_mcp_tool_classes(id: i64) -> ServerFnResult<McpToolClasses> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    let saved = db::list_mcp_tool_classes(db::get(), id).await.map_err(ServerFnError::new)?;
    // Mirrors `mcp_server_status`'s `NotConnected` short-circuit: there's
    // nothing to connect with, so don't spend a connect attempt on it.
    if config.auth_mode == "oauth" && config.oauth_credentials.is_none() {
        return Ok(McpToolClasses {
            rows: saved_only_rows(saved),
            server_note: Some("Not connected \u{2014} connect this server first".to_string()),
        });
    }
    match crate::mcp::tool_class_report(db::get(), &config).await {
        Ok(listed) => Ok(McpToolClasses { rows: merge_tool_class_rows(listed, saved), server_note: None }),
        // The page's own status section already shows these states; the
        // note here keeps the Tool-classes section consistent with it.
        Err(crate::mcp::CheckError::Unreachable(error)) => Ok(McpToolClasses {
            rows: saved_only_rows(saved),
            server_note: Some(format!("Unreachable: {error}")),
        }),
        Err(crate::mcp::CheckError::SignInExpired(error)) => Ok(McpToolClasses {
            rows: saved_only_rows(saved),
            server_note: Some(format!("Sign-in expired: {error}")),
        }),
    }
}

/// Sets (or, with `class: None`, clears) one tool's class override.
/// Reachability doesn't matter: pre-classifying a server that doesn't
/// annotate is the point, and an override for a gone tool must stay
/// clearable. An override may exist for a tool the server doesn't
/// currently list (the design keeps those); the page only ever offers
/// listed and saved names, so no free-text name reaches here. No `evict`
/// — nothing caches a class: the call reads its override fresh (SME-240).
#[post("/api/mcp-servers/{id}/tool-classes")]
pub async fn set_mcp_tool_class(
    id: i64,
    tool_name: String,
    class: Option<String>,
) -> ServerFnResult<()> {
    let config = db::get_mcp_server_config(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new(format!("no MCP server config with id {id}")))?;
    check_override_tool_name(&tool_name).map_err(ServerFnError::new)?;
    match parse_override_class(class).map_err(ServerFnError::new)? {
        Some(class) => db::set_mcp_tool_class(db::get(), config.id, &tool_name, class)
            .await
            .map_err(ServerFnError::new)?,
        None => {
            if !db::clear_mcp_tool_class(db::get(), config.id, &tool_name)
                .await
                .map_err(ServerFnError::new)?
            {
                // Can't happen from the page (it only offers rows it just
                // read); names it to the user from a second tab's clear.
                return Err(ServerFnError::new(format!("no override was saved for {tool_name}")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_http_and_https_authorization_urls_are_navigable() {
        for ok in [
            "https://auth.example.com/authorize?state=x",
            "http://127.0.0.1:9000/authorize",
            "HTTPS://auth.example.com/authorize",
        ] {
            assert!(is_navigable_authorization_url(ok), "{ok:?} should be navigable");
        }
        for bad in [
            "javascript:alert(1)//",
            "JavaScript:alert(1)",
            " javascript:alert(1)",
            "\tjavascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "java\nscript:alert(1)",
            "//evil.example.com/authorize",
            "",
            "https:",
        ] {
            assert!(!is_navigable_authorization_url(bad), "{bad:?} should be refused");
        }
    }

    /// Every status crosses to the browser as a server function's result.
    #[test]
    fn test_every_connection_status_round_trips_through_json() {
        let statuses = [
            McpConnectionStatus::Connected {
                tool_names: vec!["echo".to_string()],
            },
            McpConnectionStatus::Unreachable {
                error: "refused".to_string(),
            },
            McpConnectionStatus::NotConnected,
            McpConnectionStatus::NeedsReconnect {
                error: "use Reconnect".to_string(),
            },
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).expect("serialize");
            let back: McpConnectionStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, status);
        }
    }

    #[test]
    fn test_mcp_server_summary_exposes_header_names_but_not_values() {
        let config = db::McpServerConfig {
            id: 1,
            name: "github".to_string(),
            url: "https://api.githubcopilot.com/mcp/".to_string(),
            extra_headers: sqlx::types::Json(HashMap::from([
                (
                    "Authorization".to_string(),
                    "Bearer super-secret-token".to_string(),
                ),
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

    /// SME-241: a row whose name no tool-rule pattern can name is
    /// reported rather than broken — the edit page says so under the name
    /// field, and renaming it is the way to rule on its tools.
    #[test]
    fn test_a_server_whose_name_no_rule_can_name_is_reported_unrulable() {
        let named = |name: &str| {
            McpServerSummary::from(db::McpServerConfig {
                id: 1,
                name: name.to_string(),
                url: "https://example.invalid/mcp".to_string(),
                extra_headers: sqlx::types::Json(HashMap::new()),
                auth_mode: "static_headers".to_string(),
                oauth_credentials: None,
                oauth_client_id: None,
                oauth_client_secret: None,
                created_at: Default::default(),
                updated_at: Default::default(),
            })
        };
        assert!(named("prod/deleted").name_unrulable, "a / can't be in a pattern's server part");
        assert!(named("a*b").name_unrulable, "nor a `*` there");
        assert!(!named("github").name_unrulable, "an ordinary name is nameable");
    }

    /// Both write paths refuse a name no tool rule could name the server
    /// by (SME-241), saying the name and both grammar rules, before
    /// anything reaches the database.
    #[test]
    fn test_a_write_path_refuses_a_server_name_no_rule_can_name() {
        let no_headers = HashMap::new();
        for name in ["a/b", "a*b"] {
            let error = check_server_fields(name, "static_headers", &no_headers)
                .expect_err("{name} can't be named by a tool rule");
            assert!(error.contains(name) && error.contains('/') && error.contains('*'), "{name}: {error}");
        }
        for name in ["github", "linear.app"] {
            check_server_fields(name, "static_headers", &no_headers).expect("{name} is a name a rule can use");
        }
    }

    // --- Per-tool class overrides (SME-240) ---

    fn info(name: &str, server_class: crate::anthropic::tools::ToolClass, annotated: bool) -> crate::mcp::McpToolClassInfo {
        crate::mcp::McpToolClassInfo { name: name.to_string(), server_class, annotated }
    }

    /// Listed rows keep the server's own order and carry `server_class`
    /// and `listed: Some(true)`; saved overrides for tools the server no
    /// longer listed follow, by name, with `server_class: None` and
    /// `listed: Some(false)`; an override on a listed tool rides its row.
    #[test]
    fn test_merge_tool_class_rows_lists_first_then_stale_overrides_by_name() {
        use crate::anthropic::tools::ToolClass;
        let rows = merge_tool_class_rows(
            vec![
                info("web_search_exa", ToolClass::ReadOnly, true),
                info("edit_thing", ToolClass::Destructive, false),
            ],
            // `list_mcp_tool_classes` delivers these already by name.
            vec![
                ("old_tool".to_string(), "writing".to_string()),
                ("web_search_exa".to_string(), "destructive".to_string()),
            ],
        );
        assert_eq!(
            rows,
            vec![
                McpToolClassRow {
                    tool_name: "web_search_exa".to_string(),
                    server_class: Some("read-only".to_string()),
                    annotated: Some(true),
                    override_class: Some("destructive".to_string()),
                    listed: Some(true),
                },
                McpToolClassRow {
                    tool_name: "edit_thing".to_string(),
                    server_class: Some("destructive".to_string()),
                    annotated: Some(false),
                    override_class: None,
                    listed: Some(true),
                },
                McpToolClassRow {
                    tool_name: "old_tool".to_string(),
                    server_class: None,
                    annotated: None,
                    override_class: Some("writing".to_string()),
                    listed: Some(false),
                },
            ]
        );
    }

    /// A server with no annotations at all still merges: every listed row
    /// says the default class, and the saved set can be empty.
    #[test]
    fn test_merge_tool_class_rows_handles_the_empty_ends() {
        use crate::anthropic::tools::ToolClass;
        let rows = merge_tool_class_rows(vec![info("t", ToolClass::Destructive, false)], Vec::new());
        assert_eq!(
            rows,
            vec![McpToolClassRow {
                tool_name: "t".to_string(),
                server_class: Some("destructive".to_string()),
                annotated: Some(false),
                override_class: None,
                listed: Some(true),
            }]
        );
        let rows = merge_tool_class_rows(
            Vec::new(),
            vec![("gone".to_string(), "read-only".to_string())],
        );
        assert_eq!(
            rows,
            vec![McpToolClassRow {
                tool_name: "gone".to_string(),
                server_class: None,
                annotated: None,
                override_class: Some("read-only".to_string()),
                listed: Some(false),
            }]
        );
    }

    /// The POST's validation: a blank tool name is refused (the page only
    /// ever offers real names), and a class text outside the three — the
    /// table's CHECK's words, so a bad send says what it expected.
    #[test]
    fn test_override_input_checks_refuse_blank_names_and_unknown_classes() {
        assert_eq!(check_override_tool_name(""), Err("a tool name is required".to_string()));
        assert_eq!(check_override_tool_name("   "), Err("a tool name is required".to_string()));
        assert_eq!(check_override_tool_name("get_pr"), Ok(()));

        use crate::anthropic::tools::ToolClass;
        assert_eq!(parse_override_class(None), Ok(None));
        assert_eq!(parse_override_class(Some("read-only".to_string())), Ok(Some(ToolClass::ReadOnly)));
        assert_eq!(parse_override_class(Some("writing".to_string())), Ok(Some(ToolClass::Writing)));
        assert_eq!(parse_override_class(Some("destructive".to_string())), Ok(Some(ToolClass::Destructive)));
        assert_eq!(
            parse_override_class(Some("nope".to_string())),
            Err("unknown tool class \"nope\": expected read-only, writing or destructive".to_string())
        );
        // Not the parser's job to be forgiving: the three names only.
        assert_eq!(parse_override_class(Some("Read-Only".to_string())), Err("unknown tool class \"Read-Only\": expected read-only, writing or destructive".to_string()));
        assert_eq!(parse_override_class(Some("".to_string())), Err("unknown tool class \"\": expected read-only, writing or destructive".to_string()));
    }

    /// `ToolClass::parse` accepts exactly the three `as_str()` names
    /// (SME-240), and round-trips each.
    #[test]
    fn test_tool_class_parse_accepts_exactly_the_three_names() {
        use crate::anthropic::tools::ToolClass;
        for class in [ToolClass::ReadOnly, ToolClass::Writing, ToolClass::Destructive] {
            assert_eq!(ToolClass::parse(class.as_str()), Some(class));
        }
        for text in ["", "read only", "READ-ONLY", "destructive ", "read", "safe"] {
            assert_eq!(ToolClass::parse(text), None, "{text:?}");
        }
    }

    /// The rows cross as a server function's result.
    #[test]
    fn test_tool_classes_round_trip_through_json() {
        let classes = McpToolClasses {
            rows: vec![
                McpToolClassRow {
                    tool_name: "web_search_exa".to_string(),
                    server_class: Some("read-only".to_string()),
                    annotated: Some(true),
                    override_class: None,
                    listed: Some(true),
                },
                McpToolClassRow {
                    tool_name: "old_tool".to_string(),
                    server_class: None,
                    annotated: None,
                    override_class: Some("writing".to_string()),
                    listed: None,
                },
            ],
            server_note: Some("Unreachable: refused".to_string()),
        };
        let json = serde_json::to_string(&classes).expect("serialize");
        let back: McpToolClasses = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, classes);
    }
}
