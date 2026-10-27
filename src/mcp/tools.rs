//! The `mcp__<server>__<tool>` naming and the model's list of MCP tools.

use super::*;
use crate::anthropic::tools::ToolClass;

/// How long listing tools for a model call waits for a server that isn't
/// connected yet. A healthy server connects in a second or two; one that
/// doesn't answer is left out of this call rather than holding it up.
pub(super) const TOOL_LIST_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(1)
} else {
    std::time::Duration::from_secs(10)
};

const TOOL_NAME_PREFIX: &str = "mcp__";
const TOOL_NAME_SEPARATOR: &str = "__";

/// The name the model calls an MCP tool by: `mcp__<server>__<tool>`.
/// `pub(crate)` because a rule refusal names the tool the same way
/// (SME-241), in `crate::personas::rules`.
pub(crate) fn namespaced_tool_name(server_name: &str, tool_name: &str) -> String {
    format!("{TOOL_NAME_PREFIX}{server_name}{TOOL_NAME_SEPARATOR}{tool_name}")
}

/// Given a full smelt-facing tool name, returns `(server_name, tool_name)`
/// if it's an MCP-dispatched tool (`mcp__<server>__<tool>`) — `None`
/// otherwise, so `anthropic::tools::execute`'s dispatcher can fall through
/// to smelt's native tools. Splits on the *first* remaining `__` only, so
/// a tool name that itself contains further underscores (e.g.
/// `mcp__github__list_issues`) still resolves correctly.
pub fn parse_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(TOOL_NAME_PREFIX)?;
    rest.split_once(TOOL_NAME_SEPARATOR)
}

/// An MCP tool's class from its server's annotations (SME-237), with the
/// spec's defaults for what's absent (`readOnlyHint` false,
/// `destructiveHint` true): `readOnlyHint: true` is read-only; otherwise
/// `destructiveHint: false` is writing, and anything else destructive, so
/// a tool with no annotations is destructive. The hints are the server's
/// word; SME-240 lets the user override them.
pub(crate) fn class_from_annotations(annotations: Option<&rmcp::model::ToolAnnotations>) -> ToolClass {
    let hints = annotations.map(|a| (a.read_only_hint, a.destructive_hint));
    match hints {
        Some((Some(true), _)) => ToolClass::ReadOnly,
        Some((_, Some(false))) => ToolClass::Writing,
        _ => ToolClass::Destructive,
    }
}

/// The class of a call to `tool_name`, from its annotations in a
/// connection's tool list (`tools`). A tool the list doesn't have is
/// destructive: nothing says otherwise.
pub(super) fn listed_class(tools: &[McpTool], tool_name: &str) -> ToolClass {
    let tool = tools.iter().find(|tool| tool.name == tool_name);
    tool.map_or(ToolClass::Destructive, |tool| class_from_annotations(tool.annotations.as_ref()))
}

/// The class an MCP call — or the offer of one — is judged by: the user's
/// saved override for this server and tool when one is set, and otherwise
/// `listed`, the class the server's own annotations give it. An override
/// text that isn't a class (unreachable under the table's CHECK) is taken
/// as destructive, failing closed. One function for both of SME-241's
/// paths, so a tool is offered under exactly the class its call is
/// judged by.
pub(crate) fn judged_class(saved: Option<&str>, listed: ToolClass, server: &str, tool: &str) -> ToolClass {
    match saved {
        None => listed,
        Some(text) => ToolClass::parse(text).unwrap_or_else(|| {
            tracing::warn!(
                "the class override saved for {tool:?} on {server:?} reads {text:?}, which isn't a class; treating the call as destructive"
            );
            ToolClass::Destructive
        }),
    }
}

/// One listed tool's class information for the settings page (SME-240):
/// the class the server's own annotations give it, and whether the
/// server annotated it at all — `annotated` counts a hint, not a bare
/// title, so a title-only annotation leaves the tool at the default.
pub(crate) struct McpToolClassInfo {
    pub name: String,
    pub server_class: ToolClass,
    pub annotated: bool,
}

/// Each of the server's current tools with the class *its own
/// annotations* give it — for the settings page (SME-240), which merges
/// these with the user's saved overrides and shows each side's answer.
/// `connection_check`'s flow kept per-tool instead of name-only: the
/// user is asking, so always try (`Attempt::Always`), then one
/// `REGISTRY` read. `server_class` deliberately ignores any saved
/// override — "Server's answer" on the page means undoing back to this.
pub(crate) async fn tool_class_report(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
) -> Result<Vec<McpToolClassInfo>, CheckError> {
    if let Err(e) = ensure_connected(pool, config, Attempt::Always).await {
        if !e.sign_in_rejected {
            return Err(CheckError::Unreachable(e.message));
        }
        // As `check_connection` does for a refusal it sees itself.
        evict(config.id).await;
        return Err(CheckError::SignInExpired(e.message));
    }
    let registry = REGISTRY.lock().await;
    let conn = registry.get(&config.id).ok_or_else(|| {
        CheckError::Unreachable(format!(
            "connection to MCP server {:?} vanished immediately after connecting",
            config.name
        ))
    })?;
    Ok(conn
        .tools
        .iter()
        .map(|tool| McpToolClassInfo {
            name: tool.name.to_string(),
            server_class: class_from_annotations(tool.annotations.as_ref()),
            // Either hint counts as annotated; a title-only annotation
            // leaves the tool at the default (the plan's rule).
            annotated: tool
                .annotations
                .as_ref()
                .is_some_and(|a| a.read_only_hint.is_some() || a.destructive_hint.is_some()),
        })
        .collect())
}

fn mcp_tool_to_definition(server_name: &str, tool: &McpTool) -> ToolDefinition {
    ToolDefinition {
        name: namespaced_tool_name(server_name, tool.name.as_ref()),
        description: tool.description.clone().unwrap_or_default().into_owned(),
        input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// Builds the namespaced `ToolDefinition`s for every configured server —
/// `anthropic::tools::tool_definitions` appends these to smelt's own
/// static tool list on every `send_message` call. A server that fails to
/// connect this turn is skipped (logged, not fatal) rather than failing
/// the whole tool list — the model just doesn't see that server's tools
/// until it's reachable again. Of the tools a connected server lists, only
/// those `chain` allows (SME-241) are offered: the same evaluation, at the
/// same class, that refuses the call.
pub async fn tool_definitions_for(
    pool: &sqlx::PgPool,
    configs: &[McpServerConfig],
    chain: &crate::personas::rules::RuleChain,
) -> Vec<ToolDefinition> {
    // All servers at once, each given at most `TOOL_LIST_WAIT`. A connect
    // that takes longer carries on in the background (it's spawned, so
    // giving up on it here doesn't cancel it), and its tools show up on a
    // later call once it's connected.
    let attempts = configs.iter().map(|config| {
        let (pool, config) = (pool.clone(), config.clone());
        let name = config.name.clone();
        // In the caller's span, so its connect is the turn's child (SME-137).
        let task = tokio::spawn(tracing::Instrument::in_current_span(async move {
            ensure_connected(&pool, &config, Attempt::SkipRecentFailures).await
        }));
        async move {
            match tokio::time::timeout(TOOL_LIST_WAIT, task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    // Its URL's query can hold the server's key, and this
                    // line is exported with the turn's span (SME-137).
                    let error = crate::telemetry::scrub_urls(&e.to_string());
                    tracing::warn!(server = %name, %error, "MCP server unreachable this turn; its tools are unavailable");
                }
                Ok(Err(e)) => {
                    tracing::warn!(server = %name, error = %e, "MCP connect task failed");
                }
                Err(_) => {
                    tracing::warn!(server = %name, "MCP server still connecting; its tools are left out of this call");
                }
            }
        }
    });
    futures_util::future::join_all(attempts).await;

    // Each connected server's saved class overrides — the same rows the
    // call path reads, so a tool is offered under exactly the class its
    // call will be judged by (SME-241). Read off the `REGISTRY` guard, as
    // the call path reads them: a DB query, not connection state. A read
    // that fails leaves that server ruled by its own annotations, because
    // an offer is best-effort — the call itself still fails closed on the
    // same read.
    let connected: Vec<(i64, String)> = {
        let registry = REGISTRY.lock().await;
        configs
            .iter()
            .filter(|config| registry.contains_key(&config.id))
            .map(|config| (config.id, config.name.clone()))
            .collect()
    };
    let overrides: HashMap<i64, HashMap<String, String>> = futures_util::future::join_all(
        connected.iter().map(|(id, _)| async move { (*id, crate::db::list_mcp_tool_classes(pool, *id).await) }),
    )
    .await
    .into_iter()
    .map(|(id, saved)| match saved {
        Ok(rows) => (id, rows.into_iter().collect()),
        Err(e) => {
            let server = connected.iter().find(|(cid, _)| *cid == id).map(|(_, name)| name.as_str()).unwrap_or("?");
            tracing::warn!(server, error = %e, "couldn't read the saved class overrides; the offer falls back to the server's own annotations");
            (id, HashMap::new())
        }
    })
    .collect();

    let registry = REGISTRY.lock().await;
    configs
        .iter()
        .filter_map(|config| registry.get(&config.id).map(|conn| (config, conn)))
        .flat_map(|(config, conn)| {
            let saved = overrides.get(&config.id);
            conn.tools
                .iter()
                // SME-241: the conversation's stored chain decides which of
                // a server's tools it is offered, judged at the class its
                // call would be judged by.
                .filter(|tool| {
                    let class = judged_class(
                        saved.and_then(|rows| rows.get(tool.name.as_ref())).map(String::as_str),
                        class_from_annotations(tool.annotations.as_ref()),
                        &config.name,
                        tool.name.as_ref(),
                    );
                    chain
                        .allows_at(crate::personas::rules::ToolRef::Mcp { server: &config.name, tool: tool.name.as_ref() }, class)
                        .is_ok()
                })
                .map(|tool| mcp_tool_to_definition(&config.name, tool))
                .collect::<Vec<_>>()
        })
        .collect()
}
