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

/// The longest tool name Anthropic's API takes; it also refuses any
/// character outside `[a-zA-Z0-9_-]` (SME-365). Local servers take any
/// name, which is how an MCP tool named `search.web`, or a server named
/// `linear.app`, failed every turn only there.
pub(crate) const MAX_TOOL_NAME: usize = 128;
/// The most of those 128 a server's part may take.
const MAX_SERVER_PART: usize = 40;

/// The name the model calls an MCP tool by: `mcp__<server>__<tool>`, each
/// part made safe by `safe_part`, so every name matches
/// `^[a-zA-Z0-9_-]{1,128}$` (SME-365). A call names the tool back by it,
/// and `resolve_tool` finds the server and the tool's real name again.
/// `pub(crate)` because a rule refusal names the tool the same way
/// (SME-241), in `crate::personas::rules`.
pub(crate) fn namespaced_tool_name(server_name: &str, tool_name: &str) -> String {
    let server = safe_part(server_name, MAX_SERVER_PART);
    let room = MAX_TOOL_NAME - TOOL_NAME_PREFIX.len() - server.len() - TOOL_NAME_SEPARATOR.len();
    format!("{TOOL_NAME_PREFIX}{server}{TOOL_NAME_SEPARATOR}{}", safe_part(tool_name, room))
}

/// `name` with an MCP tool's parts made safe as `namespaced_tool_name`
/// makes them; any other name, or one already safe, as it is. For a name
/// stored before SME-365 (`mcp__linear.app__search.web`, written while a
/// local server took any name), so history sent again matches what's
/// offered now.
pub(crate) fn safe_tool_name(name: &str) -> String {
    let fits = (1..=MAX_TOOL_NAME).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if fits {
        // Already one Anthropic takes, and re-splitting it can change it
        // (a server named `__x`: SME-365 review 2).
        return name.to_string();
    }
    parse_tool_name(name).map_or_else(|| name.to_string(), |(server, tool)| namespaced_tool_name(server, tool))
}

/// `text` as it is when it's already a safe name part of at most `max`
/// characters, as nearly every name is. Otherwise each character outside
/// `[a-zA-Z0-9_-]` becomes `_`, the result is cut to fit, and `_` and six
/// hex digits of a hash of the whole of `text` go on the end: two names
/// that differ only in a refused character (`a.b`, `a b`), or only past
/// the cut, still get names of their own, and the same text always gets
/// the same name.
fn safe_part(text: &str, max: usize) -> String {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if !text.is_empty() && text.len() <= max && text.chars().all(allowed) {
        return text.to_string();
    }
    let suffix = format!("_{:06x}", fnv1a(text.as_bytes()) & 0x00ff_ffff);
    let kept: String = text
        .chars()
        .map(|c| if allowed(c) { c } else { '_' })
        .take(max.saturating_sub(suffix.len()))
        .collect();
    format!("{kept}{suffix}")
}

/// FNV-1a, 64-bit: a hash that is the same in every build and release, so
/// a tool keeps its name across a restart or an upgrade (std's hasher
/// promises neither).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Which of `configs` a model-facing MCP tool `name` belongs to, and the
/// tool's part of it: the server whose safe part, then `__`, starts what
/// follows `mcp__`. The longest such server wins, so a server named
/// `a__b` isn't taken for one named `a`.
pub(crate) fn server_for<'c, 'n>(name: &'n str, configs: &'c [McpServerConfig]) -> Option<(&'c McpServerConfig, &'n str)> {
    let rest = name.strip_prefix(TOOL_NAME_PREFIX)?;
    configs
        .iter()
        .filter_map(|config| {
            let tool = rest.strip_prefix(&safe_part(&config.name, MAX_SERVER_PART))?.strip_prefix(TOOL_NAME_SEPARATOR)?;
            Some((config, tool))
        })
        .max_by_key(|(config, _)| safe_part(&config.name, MAX_SERVER_PART).len())
}

/// The name `listed` gives the tool the model calls by tool part `part`
/// on server `server_name`: `part` itself when a listed tool has exactly
/// that name, else the listed tool whose safe name it is (SME-365), else
/// `part`, so the server answers an unknown tool as it would have.
pub(crate) fn listed_tool_name(listed: &[McpTool], server_name: &str, part: &str) -> String {
    if listed.iter().any(|tool| tool.name == part) {
        return part.to_string();
    }
    let called = namespaced_tool_name(server_name, part);
    listed
        .iter()
        .map(|tool| tool.name.as_ref())
        .find(|tool| namespaced_tool_name(server_name, tool) == called)
        .unwrap_or(part)
        .to_string()
}

/// The configured server a model-facing MCP tool `name` names, and the
/// tool's part of the name (SME-365). It connects nothing: the call maps
/// the part to the tool's real name on the connection it goes out on
/// (`call::call_tool_unspanned`).
pub(crate) async fn resolve_tool(pool: &sqlx::PgPool, name: &str) -> Result<(McpServerConfig, String), String> {
    let configs = crate::db::list_mcp_server_configs(pool)
        .await
        .map_err(|e| format!("failed to look up the MCP server for {name:?}: {e}"))?;
    match server_for(name, &configs) {
        Some((config, part)) => Ok((config.clone(), part.to_string())),
        None => {
            let server = parse_tool_name(name).map_or(name, |(server, _)| server);
            Err(format!("unknown MCP server: {server:?}"))
        }
    }
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
    /// `tool_fingerprint` of the tool listed now (SME-295).
    pub fingerprint: String,
}

/// Which tool a server lists under a name, as an override remembers it
/// (SME-295): a hash of its description and input schema, the schema's
/// object keys sorted at every level (review 1: not left to whether
/// serde_json preserves order), so a different tool listed under the same
/// name, or one whose description or arguments changed, reads as a
/// different tool, and the same tool always reads the same.
pub(crate) fn tool_fingerprint(tool: &McpTool) -> String {
    let schema = canonical_json(&serde_json::Value::Object((*tool.input_schema).clone()));
    let description = tool.description.as_deref().unwrap_or_default();
    format!("{:016x}", fnv1a(format!("{description}\u{0}{schema}").as_bytes()))
}

/// `value` as JSON with every object's keys in sorted order.
pub(super) fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}:{}", serde_json::Value::String(key.clone()), canonical_json(&map[key])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        serde_json::Value::Array(items) => format!("[{}]", items.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// Whether a saved override was set for the tool listed under its name
/// now (SME-295): one with no fingerprint counts as current, as before.
pub(crate) fn override_is_current(saved: &crate::db::SavedToolClass, listed: Option<&McpTool>) -> bool {
    match saved.tool_fingerprint.as_deref() {
        None => true,
        Some(fingerprint) => listed.is_some_and(|tool| tool_fingerprint(tool) == fingerprint),
    }
}

/// The class text a saved override gives its tool's judgment (SME-295),
/// the one rule the call, the offer and the page share. Set for the tool
/// listed now, its class. Set for a different tool, it can only narrow:
/// the more reaching of its class and the server's own (review 1: falling
/// back to the annotations alone let a server drop a user's `destructive`
/// pin by editing a description, and an override exists because the
/// annotations can't be trusted).
pub(crate) fn effective_override(saved: &crate::db::SavedToolClass, listed: Option<&McpTool>) -> String {
    if override_is_current(saved, listed) {
        return saved.class.clone();
    }
    let annotated = listed.map_or(ToolClass::Destructive, |tool| class_from_annotations(tool.annotations.as_ref()));
    match ToolClass::parse(&saved.class) {
        Some(class) => class.max(annotated).as_str().to_string(),
        None => saved.class.clone(),
    }
}

/// The fingerprint of the tool `config`'s server lists as `tool_name` now,
/// connecting first when it isn't connected (review 1: a save after a
/// restart or an evict read nothing): what an override saved now is set
/// for. `None` when the server can't be reached or doesn't list the tool.
pub(crate) async fn listed_tool_fingerprint(pool: &sqlx::PgPool, config: &McpServerConfig, tool_name: &str) -> Option<String> {
    let _ = ensure_connected(pool, config, Attempt::SkipRecentFailures).await;
    let registry = REGISTRY.lock().await;
    let conn = registry.get(&config.id)?;
    conn.tools.iter().find(|tool| tool.name == tool_name).map(tool_fingerprint)
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
            fingerprint: tool_fingerprint(tool),
        })
        .collect())
}

/// The description as the model sees it: the owning server leads, so a
/// hundred-tool list says which room each tool is in. Skips the label when
/// the description already leads with the server's name — no double label.
pub(crate) fn labelled_description(server_name: &str, description: &str) -> String {
    if description
        .to_lowercase()
        .starts_with(&server_name.to_lowercase())
    {
        description.to_string()
    } else {
        format!("[{server_name}] {description}")
    }
}

fn mcp_tool_to_definition(server_name: &str, tool: &McpTool) -> ToolDefinition {
    ToolDefinition {
        name: namespaced_tool_name(server_name, tool.name.as_ref()),
        description: labelled_description(
            server_name,
            tool.description.as_deref().unwrap_or_default(),
        ),
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
    offered_tools(pool, configs, chain)
        .await
        .iter()
        .map(|offered| mcp_tool_to_definition(&offered.server, &offered.tool))
        .collect()
}

/// One MCP tool `chain` lets a conversation call, with the class it is
/// judged at: what the model is offered, and what a script in the sandbox
/// lists (SME-359).
pub struct OfferedTool {
    pub server: String,
    pub tool: McpTool,
    pub class: ToolClass,
}

/// [`tool_definitions_for`]'s tools before they become definitions: every
/// connected server's tools that `chain` allows, each at its judged class.
pub async fn offered_tools(
    pool: &sqlx::PgPool,
    configs: &[McpServerConfig],
    chain: &crate::personas::rules::RuleChain,
) -> Vec<OfferedTool> {
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
    let overrides: HashMap<i64, HashMap<String, crate::db::SavedToolClass>> = futures_util::future::join_all(
        connected.iter().map(|(id, _)| async move { (*id, crate::db::list_mcp_tool_classes(pool, *id).await) }),
    )
    .await
    .into_iter()
    .map(|(id, saved)| match saved {
        Ok(rows) => (id, rows.into_iter().map(|row| (row.tool_name.clone(), row)).collect()),
        Err(e) => {
            let server = connected.iter().find(|(cid, _)| *cid == id).map(|(_, name)| name.as_str()).unwrap_or("?");
            tracing::warn!(server, error = %e, "couldn't read the saved class overrides; the offer falls back to the server's own annotations");
            (id, HashMap::new())
        }
    })
    .collect();

    let registry = REGISTRY.lock().await;
    // An override saved with no fingerprint (before SME-295, or for a tool
    // not listed then) takes the tool listed now as the one it's for
    // (review 1). That's never looser than leaving it unfingerprinted,
    // which decides for whatever tool is behind the name; from then on, a
    // change of tool can only narrow it (review 2). Best-effort, off the
    // turn.
    let adopt: Vec<(i64, String, String)> = configs
        .iter()
        .filter_map(|config| registry.get(&config.id).map(|conn| (config, conn)))
        .flat_map(|(config, conn)| {
            let saved = overrides.get(&config.id);
            conn.tools
                .iter()
                .filter(|tool| saved.and_then(|rows| rows.get(tool.name.as_ref())).is_some_and(|row| row.tool_fingerprint.is_none()))
                .map(|tool| (config.id, tool.name.to_string(), tool_fingerprint(tool)))
                .collect::<Vec<_>>()
        })
        .collect();
    if !adopt.is_empty() {
        let pool = pool.clone();
        tokio::spawn(async move {
            for (server_id, tool, fingerprint) in adopt {
                if let Err(e) = crate::db::adopt_mcp_tool_fingerprint(&pool, server_id, &tool, &fingerprint).await {
                    tracing::warn!(server_id, tool, error = %e, "couldn't record which tool a class override is for");
                }
            }
        });
    }
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
                .filter_map(|tool| {
                    // An override set for a different tool under this name
                    // can only narrow it (SME-295).
                    let saved = saved.and_then(|rows| rows.get(tool.name.as_ref())).map(|row| effective_override(row, Some(tool)));
                    let class = judged_class(
                        saved.as_deref(),
                        class_from_annotations(tool.annotations.as_ref()),
                        &config.name,
                        tool.name.as_ref(),
                    );
                    chain
                        .allows_at(crate::personas::rules::ToolRef::Mcp { server: &config.name, tool: tool.name.as_ref() }, class)
                        .is_ok()
                        .then(|| OfferedTool { server: config.name.clone(), tool: tool.clone(), class })
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
