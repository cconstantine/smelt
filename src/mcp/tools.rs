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

pub(super) fn namespaced_tool_name(server_name: &str, tool_name: &str) -> String {
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

/// The class of a call to `tool_name` on `config`'s server, from the
/// annotations in the connection's tool list. Connects or re-lists first
/// as a model call would (`ensure_connected`), so a call right after an
/// edit's `evict` or a `tools/list_changed` gets the tool's current class.
/// A server that can't be reached, or a tool it doesn't list, is
/// destructive: nothing says otherwise.
pub(crate) async fn tool_class(pool: &sqlx::PgPool, config: &McpServerConfig, tool_name: &str) -> ToolClass {
    if ensure_connected(pool, config, Attempt::SkipRecentFailures).await.is_err() {
        return ToolClass::Destructive;
    }
    let registry = REGISTRY.lock().await;
    let tool = registry.get(&config.id).and_then(|conn| conn.tools.iter().find(|tool| tool.name == tool_name));
    tool.map_or(ToolClass::Destructive, |tool| class_from_annotations(tool.annotations.as_ref()))
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
/// until it's reachable again.
pub async fn tool_definitions_for(
    pool: &sqlx::PgPool,
    configs: &[McpServerConfig],
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

    let registry = REGISTRY.lock().await;
    configs
        .iter()
        .filter_map(|config| registry.get(&config.id).map(|conn| (config, conn)))
        .flat_map(|(config, conn)| {
            conn.tools
                .iter()
                .map(|tool| mcp_tool_to_definition(&config.name, tool))
                .collect::<Vec<_>>()
        })
        .collect()
}
