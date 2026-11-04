//! Script MCP calls, smelt's side (SME-359): a script in a sandbox pod
//! asks the agent's Unix socket, the agent sends `mcp_request` up the
//! WebSocket smelt dialed to that pod, and this answers it with
//! `mcp_response` on the same connection.
//!
//! **The connection is the identity.** A request carries no conversation
//! or pod id: it is ruled as the conversation of the connection it came
//! on, resolved when smelt dialed the pod (`TerminalConnection`). The call
//! then goes through exactly what a model's call goes through
//! (`anthropic::tools::call_mcp_for_script`: the stored rule chain, failing
//! closed, and the class check on the connection the call goes out on);
//! the user's answer to SME-359's Q1 was "same as the model".
//!
//! Each call is a `sandbox_mcp_calls` row (never its arguments or result),
//! which the sandbox panel and the model's terminal tools summarize per
//! command.

use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::agent_protocol::{ClientMessage, McpRequest};
use crate::db;

use super::agent::TerminalConnection;

/// The most of a result's JSON a script gets: past the model's 30k cap on
/// purpose (not reading every result is the point), and far under the
/// WebSocket's 64 MiB message cap.
pub(crate) const MAX_RESULT_BYTES: usize = 8 << 20;

/// The status and JSON body a script gets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Answer {
    pub status: u16,
    pub body: String,
}

impl Answer {
    fn json(status: u16, body: &Value) -> Self {
        Self { status, body: body.to_string() }
    }

    fn error(status: u16, message: impl std::fmt::Display) -> Self {
        Self::json(status, &json!({ "error": message.to_string() }))
    }
}

/// What the call came from: the connection's conversation and pod, and
/// the command the agent traced the caller to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Caller<'a> {
    pub conversation_id: i64,
    pub pod_id: i64,
    pub command_id: Option<&'a str>,
}

/// Answers one `mcp_request` from `conn`'s agent, in a task of its own:
/// the connection's reader loop never waits on a call. The answer goes
/// back on the connection the request came on; one for a connection that
/// has since closed is dropped (the agent has already told the script).
pub(super) fn spawn_request(pool: PgPool, conn: Arc<TerminalConnection>, call_id: u64, command_id: Option<String>, request: McpRequest) {
    tokio::spawn(async move {
        let caller = Caller { conversation_id: conn.conversation_id, pod_id: conn.pod_id, command_id: command_id.as_deref() };
        let answer = handle(&pool, caller, request).await;
        if let Err(e) = conn.send(&ClientMessage::McpResponse { call_id, status: answer.status, body: answer.body }) {
            tracing::debug!(call_id, error = %e, "the script's MCP call ended after its connection did; its answer is dropped");
        }
    });
}

/// One request's answer, in a `script mcp call` span: the conversation,
/// pod and command, the call's class and outcome, never its arguments or
/// result (SME-137).
pub(crate) async fn handle(pool: &PgPool, caller: Caller<'_>, request: McpRequest) -> Answer {
    handle_with(pool, caller, request, &Limits::deployment()).await
}

/// [`handle`] under `limits` rather than the deployment's.
pub(crate) async fn handle_with(pool: &PgPool, caller: Caller<'_>, request: McpRequest, limits: &Limits) -> Answer {
    use tracing::field::Empty;
    let span = tracing::info_span!(
        "script mcp call",
        conversation_id = caller.conversation_id,
        pod_id = caller.pod_id,
        command_id = caller.command_id.unwrap_or_default(),
        smelt.script.request = match &request { McpRequest::List { .. } => "list", McpRequest::Call { .. } => "call" },
        smelt.mcp.server = Empty,
        smelt.mcp.tool = Empty,
        smelt.tool.class = Empty,
        smelt.script.status = Empty,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        let answer = match request {
            McpRequest::List { server } => list(pool, caller, server.as_deref(), limits).await,
            McpRequest::Call { body } => call(pool, caller, &span, &body, limits).await,
        };
        span.record("smelt.script.status", answer.status as i64);
        if answer.status >= 400 {
            crate::telemetry::mark_error(&span, &format!("answered {}", answer.status));
        }
        answer
    })
    .await
}

/// The checks every request passes before anything is read or sent:
/// the pod is still the conversation's live pod, and an agent
/// conversation is still active.
async fn check_caller(pool: &PgPool, caller: Caller<'_>) -> Result<(), Answer> {
    match db::sandbox_pod_is_live_for(pool, caller.pod_id, caller.conversation_id).await {
        Ok(true) => {}
        Ok(false) => return Err(Answer::error(503, "this sandbox is being stopped")),
        Err(e) => return Err(Answer::error(503, format!("couldn't check this sandbox: {e}"))),
    }
    match db::agent_state(pool, caller.conversation_id).await {
        Ok(None) => Ok(()),
        Ok(Some(state)) if state == "active" => Ok(()),
        Ok(Some(state)) => Err(Answer::error(403, format!("this agent is {state}, so its sandbox's scripts can't call MCP tools"))),
        Err(e) => Err(Answer::error(503, format!("couldn't check this conversation's agent: {e}"))),
    }
}

/// The configured server and the tool's part of `name`: `<server>/<tool>`
/// (the tool-rule grammar's name; the longest configured server name
/// before a `/` wins) or the model's `mcp__<server>__<tool>`.
async fn resolve(pool: &PgPool, name: &str) -> Result<(db::McpServerConfig, String), Answer> {
    let (config, part, by_model_name) = resolve_server(pool, name).await?;
    match crate::mcp::listed_tool_for_script(pool, &config, &part, by_model_name).await {
        Ok(Some(listed)) => Ok((config, listed)),
        Ok(None) => Err(Answer::error(404, format!("{:?} lists no tool {name:?}: name it as `smelt-mcp tools` lists it", config.name))),
        Err(e) => Err(Answer::error(502, e)),
    }
}

/// The configured server `name` names, the tool's part of it, and whether
/// it was the model's `mcp__…` form.
async fn resolve_server(pool: &PgPool, name: &str) -> Result<(db::McpServerConfig, String, bool), Answer> {
    if crate::mcp::parse_tool_name(name).is_some() {
        let (config, part) = crate::mcp::resolve_tool(pool, name).await.map_err(|e| Answer::error(404, e))?;
        return Ok((config, part, true));
    }
    let configs = db::list_mcp_server_configs(pool)
        .await
        .map_err(|e| Answer::error(503, format!("couldn't read the MCP servers: {e}")))?;
    configs
        .into_iter()
        .filter_map(|config| {
            let tool = name.strip_prefix(config.name.as_str())?.strip_prefix('/')?.to_string();
            (!tool.is_empty()).then_some((config, tool, false))
        })
        .max_by_key(|(config, _, _)| config.name.len())
        .ok_or_else(|| Answer::error(404, format!("no MCP tool {name:?}: name it <server>/<tool>, as `smelt-mcp tools` lists it")))
}

/// The longest `tool` a request may name (review round 1, M2): every name
/// a server lists is far shorter, and the name is logged and recorded.
const MAX_TOOL_NAME_BYTES: usize = 256;

/// The tool name and arguments of a `POST /v1/call` body.
fn parse_call(body: &str) -> Result<(String, Value), Answer> {
    let value: Value = serde_json::from_str(body).map_err(|e| Answer::error(400, format!("the request body isn't JSON: {e}")))?;
    let tool = value
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| Answer::error(400, "the request needs \"tool\": \"<server>/<tool>\""))?
        .to_string();
    if tool.len() > MAX_TOOL_NAME_BYTES {
        return Err(Answer::error(400, format!("\"tool\" is {} bytes, over the {MAX_TOOL_NAME_BYTES}-byte limit", tool.len())));
    }
    let arguments = match value.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(arguments @ Value::Object(_)) => arguments.clone(),
        Some(other) => return Err(Answer::error(400, format!("\"arguments\" must be a JSON object, got {other}"))),
    };
    Ok((tool, arguments))
}

async fn call(pool: &PgPool, caller: Caller<'_>, span: &tracing::Span, body: &str, limits: &Limits) -> Answer {
    let (name, arguments) = match parse_call(body) {
        Ok(call) => call,
        Err(answer) => return answer,
    };
    let (config, tool) = match resolve(pool, &name).await {
        Ok(resolved) => resolved,
        Err(answer) => return answer,
    };
    span.record("smelt.mcp.server", config.name.as_str());
    if let Err(answer) = check_caller(pool, caller).await {
        return answer;
    }
    if !limits.enabled() {
        return turned_off();
    }
    // Held for the whole call: its place among those in flight.
    let _slot = match take_limits(pool, caller, limits).await {
        Ok(slot) => slot,
        Err(held_back) => {
            // Counted, not a row: a loop spinning on 429s would otherwise
            // write a row per refusal (review round 1, M2).
            if let Some(command_id) = caller.command_id {
                note_held_back(caller.conversation_id, command_id);
                publish_summary_soon(pool, caller.conversation_id, command_id);
            }
            return held_back;
        }
    };
    let row = match db::insert_sandbox_mcp_call(pool, caller.conversation_id, caller.pod_id, caller.command_id, &config.name, &tool, None, "running").await {
        Ok(row) => row,
        // Failing closed: a call that can't be recorded can't be counted
        // against its limits either.
        Err(e) => return Answer::error(503, format!("couldn't record the call: {e}")),
    };

    let made = crate::anthropic::tools::call_mcp_for_script(pool, caller.conversation_id, &config, &tool, arguments).await;
    let class = made.class.map(|class| class.as_str());
    if let Some(class) = class {
        span.record("smelt.tool.class", class);
    }
    span.record("smelt.mcp.tool", made.tool.as_deref().unwrap_or(&tool));
    let (outcome, answer) = match made.result {
        Ok(result) => {
            let is_error = result.is_error.unwrap_or(false);
            let mut body = json!({
                "content": result.content,
                "structured_content": result.structured_content,
                "text": crate::mcp::result_text(&result),
            });
            if is_error {
                body["is_error"] = Value::Bool(true);
            }
            let outcome = if is_error { "tool_error" } else { "ok" };
            let body = body.to_string();
            if body.len() > MAX_RESULT_BYTES {
                (outcome, Answer::error(502, format!("the result is {} bytes, over the {MAX_RESULT_BYTES}-byte limit for a script's call", body.len())))
            } else {
                (outcome, Answer { status: if is_error { 422 } else { 200 }, body })
            }
        }
        Err(crate::anthropic::tools::ScriptMcpError::Refused(message)) => ("refused", Answer::error(403, message)),
        Err(crate::anthropic::tools::ScriptMcpError::Failed(message)) => ("failed", Answer::error(502, message)),
    };
    if let Err(e) = db::finish_sandbox_mcp_call(pool, row, class, outcome).await {
        tracing::warn!(row, error = %e, "couldn't record how a script's MCP call ended");
    }
    if let Some(command_id) = caller.command_id {
        publish_summary_soon(pool, caller.conversation_id, command_id);
    }
    answer
}

/// The most often one command's summary is published while its calls
/// finish: a fast loop doesn't flood the event stream.
pub(crate) const SUMMARY_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// Publishes `command_id`'s summary `SUMMARY_EVERY` from now, unless that
/// is already set to happen: the trailing edge, so the last call is
/// always in a published summary.
fn publish_summary_soon(pool: &PgPool, conversation_id: i64, command_id: &str) {
    static PENDING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    if !PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(command_id.to_string()) {
        return;
    }
    let (pool, command_id) = (pool.clone(), command_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(SUMMARY_EVERY).await;
        // Off the list before the read, so a call that finishes during it
        // sets up the next publish rather than going unsaid.
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).remove(&command_id);
        match command_summary(&pool, conversation_id, &command_id).await {
            Ok(Some(summary)) => {
                crate::events::publish(conversation_id, crate::events::ConversationEvent::SandboxMcpCalls { command_id, summary })
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(%command_id, error = %e, "couldn't read a command's MCP call summary to publish"),
        }
    });
}

/// The command's summary, with the calls held back by a limit (review
/// round 1, M2: counted in memory, not a row each).
pub(crate) async fn command_summary(
    pool: &PgPool,
    conversation_id: i64,
    command_id: &str,
) -> Result<Option<crate::api::mcp_calls::McpCallSummary>, sqlx::Error> {
    let mut counts = db::sandbox_mcp_call_counts(pool, &[command_id.to_string()]).await?.remove(command_id).unwrap_or_default();
    let held_back = held_back(conversation_id, command_id);
    if held_back > 0 {
        counts.push(crate::api::mcp_calls::McpCallCount {
            server: String::new(),
            tool: String::new(),
            outcome: "limited".to_string(),
            stale: false,
            count: held_back,
        });
    }
    Ok(crate::api::mcp_calls::McpCallSummary::from_counts(&counts))
}

/// The most commands whose held-back calls are counted at once. Past it
/// the counts start again: they are a note on the panel, not a record.
const MAX_HELD_BACK_COMMANDS: usize = 10_000;

/// Calls held back by a limit, per conversation and command, since smelt
/// started.
static HELD_BACK: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<(i64, String), i64>>> =
    std::sync::LazyLock::new(Default::default);

fn note_held_back(conversation_id: i64, command_id: &str) {
    let mut held = HELD_BACK.lock().unwrap_or_else(|e| e.into_inner());
    if held.len() >= MAX_HELD_BACK_COMMANDS && !held.contains_key(&(conversation_id, command_id.to_string())) {
        held.clear();
    }
    *held.entry((conversation_id, command_id.to_string())).or_default() += 1;
}

fn held_back(conversation_id: i64, command_id: &str) -> i64 {
    HELD_BACK.lock().unwrap_or_else(|e| e.into_inner()).get(&(conversation_id, command_id.to_string())).copied().unwrap_or(0)
}

/// What a script gets when the deployment has turned script calls off.
fn turned_off() -> Answer {
    Answer::error(403, format!("script MCP calls are turned off on this deployment ({}=0)", limits::CALLS_PER_MINUTE_ENV))
}

/// The per-command budget, then a slot and a token (`limits::take`). A
/// call held back is a 429, with a time to retry after when waiting helps.
async fn take_limits(pool: &PgPool, caller: Caller<'_>, limits: &Limits) -> Result<limits::Slot, Answer> {
    let held_back = |held: limits::Limited| {
        let mut body = json!({ "error": held.message });
        if let Some(secs) = held.retry_after_secs {
            body["retry_after_secs"] = Value::from(secs);
        }
        Answer::json(429, &body)
    };
    let made = db::count_sandbox_mcp_calls_against_budget(pool, caller.conversation_id, caller.pod_id, caller.command_id)
        .await
        .map_err(|e| Answer::error(503, format!("couldn't count this command's MCP calls: {e}")))?;
    if made >= limits.per_command {
        let whose = if caller.command_id.is_some() { "this command" } else { "this sandbox's processes outside any command" };
        return Err(held_back(limits::Limited {
            message: format!(
                "{whose} made {made} MCP calls, its limit ({}); a new terminal command starts a new budget",
                limits::CALLS_PER_COMMAND_ENV
            ),
            retry_after_secs: None,
        }));
    }
    limits::take(caller.conversation_id, limits).map_err(held_back)
}

/// `GET /v1/tools`: what this conversation's scripts may call, as the
/// model is offered it (the same chain and class), with the servers' own
/// names and input schemas. Best-effort, like the offer: the call is the
/// check.
async fn list(pool: &PgPool, caller: Caller<'_>, server: Option<&str>, limits: &Limits) -> Answer {
    if let Err(answer) = check_caller(pool, caller).await {
        return answer;
    }
    if !limits.enabled() {
        return turned_off();
    }
    let chain = match db::agent_rule_chain(pool, caller.conversation_id).await {
        Ok(stored) => crate::personas::rules::RuleChain::from_stored(stored),
        Err(e) => return Answer::error(503, format!("couldn't read this conversation's tool rules: {e}")),
    };
    let mut configs = match db::list_mcp_server_configs(pool).await {
        Ok(configs) => configs,
        Err(e) => return Answer::error(503, format!("couldn't read the MCP servers: {e}")),
    };
    if let Some(server) = server {
        configs.retain(|config| config.name == server);
        if configs.is_empty() {
            return Answer::error(404, format!("no MCP server {server:?}"));
        }
    }
    let tools: Vec<Value> = crate::mcp::offered_tools(pool, &configs, &chain)
        .await
        .into_iter()
        .map(|offered| {
            json!({
                "name": format!("{}/{}", offered.server, offered.tool.name),
                "server": offered.server,
                "tool": offered.tool.name,
                "class": offered.class.as_str(),
                "description": offered.tool.description,
                "input_schema": offered.tool.input_schema,
            })
        })
        .collect();
    Answer::json(200, &json!({ "tools": tools }))
}

mod limits;
pub(crate) use limits::Limits;
pub(crate) use limits::Limits as ScriptMcpLimits;

#[cfg(test)]
mod tests;
