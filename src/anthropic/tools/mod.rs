//! smelt's own tools, dispatched by name: the sandbox (pods, terminals,
//! commands, files, language servers), the todo list, git, the web, and
//! browsing sessions. MCP servers' tools are dispatched from here too.

use serde::{Deserialize, Serialize};

/// One entry in a conversation's model-visible todo list (the
/// `todowrite`/`todoread` tools). Defined outside the `server`-gated
/// module below because it crosses the client/server boundary as a
/// server-function return value (`api::chat::get_todos`) and as a
/// live-event payload
/// (`events::ConversationEvent::TodoListUpdate`), both of which the `web`
/// target compiles too. See SME-20.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// The same snake_case spelling serde uses on the wire — for contexts
    /// (like a compaction summarization prompt's plain-text description)
    /// that want the string without going through `serde_json`.
    /// Server-only: its one caller, `describe_live_state`, is itself
    /// `#[cfg(feature = "server")]`.
    #[cfg(feature = "server")]
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }
}

/// One native tool: what the model is told (`def`), what runs when it
/// calls it (`run`) and what a call does outside its sandbox (`class`,
/// SME-237). Each domain module lists its own in `tools()`, so a tool's
/// definition, its dispatch and its class sit side by side and can't
/// drift apart (SME-54).
#[cfg(feature = "server")]
struct Tool {
    def: crate::anthropic::ToolDefinition,
    run: Run,
    class: Class,
}

/// Who a tool is offered to (SME-236): every conversation, only one
/// without a parent (the user's own), or only an agent. `Linked`
/// (SME-238) is offered to every conversation, so the tool list doesn't
/// change when a conversation starts its first agent (which would rewrite
/// its whole cached prompt), but `execute` lets only one with a parent or
/// agents of its own call it. `execute` checks every offer from the
/// database: a call can come from a history written before the
/// conversation changed.
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Offer {
    All,
    Root,
    Child,
    Linked,
}

/// A tool in the registry, with who it's offered to.
#[cfg(feature = "server")]
struct Registered {
    tool: Tool,
    offer: Offer,
}

/// What one tool call brings in.
#[cfg(feature = "server")]
#[derive(Clone, Copy)]
struct Call<'a> {
    pool: &'a sqlx::PgPool,
    conversation_id: i64,
    /// Only `run_terminal_command` uses it (as the command's id).
    #[allow(dead_code)]
    tool_use_id: &'a str,
    input: &'a serde_json::Value,
}

#[cfg(feature = "server")]
type ToolFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;

#[cfg(feature = "server")]
type Run = for<'a> fn(Call<'a>) -> ToolFuture<'a>;

/// A tool's `run`, from an expression using the call `c`:
/// `run!(|c| read_file_tool(c.pool, c.conversation_id, c.input))`.
#[cfg(feature = "server")]
macro_rules! run {
    (|$c:ident| $body:expr) => {{
        fn run<'a>($c: super::Call<'a>) -> super::ToolFuture<'a> {
            Box::pin(async move { $body.await })
        }
        run as super::Run
    }};
}

#[cfg(feature = "server")]
mod agents;
#[cfg(feature = "server")]
mod class;
#[cfg(feature = "server")]
use class::Class;
#[cfg(feature = "server")]
pub(crate) use class::ToolClass;
#[cfg(feature = "server")]
mod browser;
#[cfg(feature = "server")]
mod files;
#[cfg(feature = "server")]
mod git;
#[cfg(feature = "server")]
mod lsp;
#[cfg(feature = "server")]
mod pods;
#[cfg(feature = "server")]
mod questions;
#[cfg(feature = "server")]
mod todos;
#[cfg(feature = "server")]
mod web;

/// Every native tool, in the order the model is offered them. The order is
/// part of what the model sees, so it stays fixed: changing it changes the
/// request like adding or removing a tool would. The agent tools go last,
/// so every older tool keeps its place.
#[cfg(feature = "server")]
static REGISTRY: std::sync::LazyLock<Vec<Registered>> = std::sync::LazyLock::new(|| {
    [
        pods::tools(),
        files::tools(),
        todos::tools(),
        git::tools(),
        web::tools(),
        browser::tools(),
        lsp::tools(),
        questions::tools(),
    ]
        .into_iter()
        .flatten()
        .map(|tool| Registered { tool, offer: Offer::All })
        .chain(agents::tools())
        .collect()
});

#[cfg(feature = "server")]
mod server {
    use serde_json::Value;
    use sqlx::PgPool;


    /// Runs the named tool against `input`, returning its result as a plain
    /// string on success or an error message on failure — the caller (the
    /// `send_message` turn loop) wraps either into a `ContentBlock::ToolResult`.
    /// `tool_use_id` is only meaningful to `run_terminal_command`, which
    /// reuses it as the command's id.
    /// `pool` is threaded through (rather than reached for via `db::get()`)
    /// so tests can use an isolated `#[sqlx::test]` pool end-to-end. Every
    /// result, success or error, is capped at `MAX_TOOL_RESULT_CHARS`
    /// (`cap_tool_result`).
    pub async fn execute(
        pool: &PgPool,
        conversation_id: i64,
        tool_use_id: &str,
        name: &str,
        input: &Value,
    ) -> Result<String, String> {
        use tracing::field::Empty;
        // The call's ids and outcome, never its input or output (SME-137).
        let span = tracing::info_span!(
            "tool",
            otel.name = %format_args!("tool {name}"),
            conversation_id,
            gen_ai.operation.name = "execute_tool",
            gen_ai.tool.name = %name,
            gen_ai.tool.call.id = %tool_use_id,
            smelt.tool.mcp_server = Empty,
            smelt.tool.class = Empty,
            smelt.tool.truncated = Empty,
            otel.status_code = Empty,
            otel.status_description = Empty,
        );
        if let Some((server_name, _)) = crate::mcp::parse_tool_name(name) {
            span.record("smelt.tool.mcp_server", server_name);
        }
        crate::telemetry::in_span(span.clone(), async move {
            let result = execute_uncapped(pool, &span, conversation_id, tool_use_id, name, input).await;
            let (result, truncated) = match result {
                Ok(output) => {
                    let (output, truncated) = cap_tool_result(output);
                    (Ok(output), truncated)
                }
                Err(message) => {
                    let (message, truncated) = cap_tool_result(message);
                    // An MCP tool's error can be its own output: its
                    // `mcp call` span has what smelt can say (SME-137).
                    let is_mcp = crate::mcp::parse_tool_name(name).is_some();
                    crate::telemetry::mark_error(&span, if is_mcp { MCP_TOOL_FAILED } else { &message });
                    (Err(message), truncated)
                }
            };
            span.record("smelt.tool.truncated", truncated);
            result
        })
        .await
    }

    /// An MCP tool call's failure on its `tool` span (SME-137 review 1).
    pub(super) const MCP_TOOL_FAILED: &str = "the MCP tool call failed (see its mcp call span)";

    /// The most of one tool result that reaches the model (SME-76).
    /// Conversation 23 took in single results of 40–106 KB, which on a
    /// local model meant 10–30 minutes per step. `pub(super)` because a
    /// tool's own size test guards against it (SME-189 review 1): `read_file`'s
    /// cut-line budget is set so a page of cut lines stays under this.
    pub(super) const MAX_TOOL_RESULT_CHARS: usize = 30_000;

    /// Cuts `result` to `MAX_TOOL_RESULT_CHARS`, saying so and how to see
    /// the rest. The rest is dropped. Also says whether it cut.
    fn cap_tool_result(result: String) -> (String, bool) {
        let total = result.chars().count();
        match crate::fetch_guard::truncate(result, MAX_TOOL_RESULT_CHARS) {
            (kept, true) => (
                format!(
                    "{kept}\n[cut to the first {MAX_TOOL_RESULT_CHARS} of {total} characters; narrow \
                     the request (a smaller query, offset/limit, tail_lines) to see the rest]"
                ),
                true,
            ),
            (whole, false) => (whole, false),
        }
    }

    /// The tool a call names: one of smelt's, or an MCP server's.
    enum Target<'a> {
        Native(&'static super::Registered),
        Mcp { config: crate::db::McpServerConfig, tool: &'a str },
    }

    /// Finds the tool `name` names. An unknown tool, or an MCP server
    /// that isn't configured, is a plain tool error: the config could have
    /// been deleted between when `tool_definitions` last offered the tool
    /// and when the model called it.
    async fn resolve<'a>(pool: &PgPool, name: &'a str) -> Result<Target<'a>, String> {
        if let Some((server_name, tool)) = crate::mcp::parse_tool_name(name) {
            let config = crate::db::get_mcp_server_config_by_name(pool, server_name)
                .await
                .map_err(|e| format!("failed to look up MCP server {server_name:?}: {e}"))?
                .ok_or_else(|| format!("unknown MCP server: {server_name:?}"))?;
            return Ok(Target::Mcp { config, tool });
        }
        super::REGISTRY
            .iter()
            .find(|registered| registered.tool.def.name == name)
            .map(Target::Native)
            .ok_or_else(|| format!("unknown tool: {name}"))
    }

    async fn execute_uncapped(
        pool: &PgPool,
        span: &tracing::Span,
        conversation_id: i64,
        tool_use_id: &str,
        name: &str,
        input: &Value,
    ) -> Result<String, String> {
        // The call's class is what SME-241 will refuse it by: a native
        // tool's from its `class` and the input, an MCP tool's from its
        // server's annotations, on the connection the call goes out on.
        let record = |class: super::ToolClass| {
            span.record("smelt.tool.class", class.as_str());
        };
        match resolve(pool, name).await? {
            Target::Mcp { config, tool } => crate::mcp::call_tool_classed(pool, &config, tool, input.clone(), record).await,
            Target::Native(registered) => {
                record(registered.tool.class.of_call(input));
                if registered.offer != super::Offer::All {
                    offered_to(pool, conversation_id, name, registered.offer).await?;
                }
                (registered.tool.run)(super::Call { pool, conversation_id, tool_use_id, input }).await
            }
        }
    }

    /// Refuses a call to a tool this conversation isn't offered (SME-236),
    /// read from the database rather than from what was offered.
    async fn offered_to(pool: &PgPool, conversation_id: i64, name: &str, offer: super::Offer) -> Result<(), String> {
        let (parent, has_children) = crate::db::conversation_links(pool, conversation_id)
            .await
            .map_err(|e| format!("couldn't check who may call {name}: {e}"))?;
        match (offer, parent) {
            (super::Offer::Root, Some(_)) => Err(format!("{name} is only for a conversation without a parent")),
            (super::Offer::Child, None) => Err(format!("{name} is only for an agent")),
            (super::Offer::Linked, None) if !has_children => {
                Err(format!("{name} is only for a conversation with a parent or agents of its own"))
            }
            _ => Ok(()),
        }
    }

    /// Whether a tool offered as `offer` goes to a conversation with
    /// (`has_parent`) or without a parent.
    fn is_offered(offer: super::Offer, has_parent: bool) -> bool {
        match offer {
            super::Offer::All | super::Offer::Linked => true,
            super::Offer::Root => !has_parent,
            super::Offer::Child => has_parent,
        }
    }

    /// smelt's own built-in tool schemas — everything except MCP-provided
    /// tools (see `tool_definitions` below, which appends those).
    /// Deliberately synchronous and pool-free: a caller that only needs
    /// smelt's own static tools (tests) shouldn't pay for a DB round trip
    /// and MCP connection attempts it doesn't need. Every tool, whoever
    /// it's offered to.
    #[cfg(test)]
    pub fn native_tool_definitions() -> Vec<crate::anthropic::ToolDefinition> {
        super::REGISTRY.iter().map(|registered| registered.tool.def.clone()).collect()
    }

    /// The full tool list offered to the model on every `send_message`
    /// call: smelt's own static tools plus every configured MCP server's
    /// tools, namespaced `mcp__<server_name>__<tool_name>` (see
    /// `crate::mcp`). A server that fails to connect this turn is skipped
    /// rather than failing the whole request — see
    /// `crate::mcp::tool_definitions_for`. Only the tools offered to
    /// `conversation_id` (SME-236): `start_agent` to a conversation without
    /// a parent, `finish` and `ask_parent` to an agent. A failed read
    /// offers neither.
    pub async fn tool_definitions(pool: &PgPool, conversation_id: i64) -> Vec<crate::anthropic::ToolDefinition> {
        let has_parent = match crate::db::conversation_parent(pool, conversation_id).await {
            Ok(parent) => Some(parent.is_some()),
            Err(e) => {
                tracing::warn!(conversation_id, error = %e, "couldn't read the conversation's parent; its agent tools are left out this turn");
                None
            }
        };
        let mut definitions: Vec<crate::anthropic::ToolDefinition> = super::REGISTRY
            .iter()
            .filter(|registered| match has_parent {
                Some(has_parent) => is_offered(registered.offer, has_parent),
                None => matches!(registered.offer, super::Offer::All | super::Offer::Linked),
            })
            .map(|registered| registered.tool.def.clone())
            .collect();
        match crate::db::list_mcp_server_configs(pool).await {
            Ok(configs) => {
                definitions.extend(crate::mcp::tool_definitions_for(pool, &configs).await)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to list configured MCP servers; their tools are unavailable this turn")
            }
        }
        definitions
    }

    pub(super) fn required_str(input: &Value, field: &str) -> Result<String, String> {
        input
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("missing or non-string field: {field}"))
    }

    pub(super) fn required_i64(input: &Value, field: &str) -> Result<i64, String> {
        input
            .get(field)
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("missing or non-integer field: {field}"))
    }

    /// An optional idle-notice interval: `None` when absent or an explicit
    /// null, which asks for the deployment default rather than an error
    /// (`run_terminal_command`, SME-79). Present but not a usable interval
    /// is refused naming the field.
    pub(super) fn optional_notice_secs(input: &Value, field: &str) -> Result<Option<i32>, String> {
        match input.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => notice_secs(value, field).map(Some),
        }
    }

    /// A required idle-notice interval: `set_idle_notice` has no default to
    /// fall back to, so `0` has to be said out loud (SME-79). Same rules as
    /// `optional_notice_secs`.
    pub(super) fn required_notice_secs(input: &Value, field: &str) -> Result<i32, String> {
        let value = input
            .get(field)
            .ok_or_else(|| format!("missing field: {field}"))?;
        notice_secs(value, field)
    }

    /// What makes a JSON value a usable idle-notice interval (SME-79): a
    /// whole number of seconds from zero up — `0` meaning "never", a choice
    /// rather than a mistake — and small enough for the column that stores
    /// it. Every refusal names the field, so the model can fix its call.
    fn notice_secs(value: &Value, field: &str) -> Result<i32, String> {
        let secs = value
            .as_i64()
            .ok_or_else(|| format!("{field} must be an integer, got {value}"))?;
        if secs < 0 {
            return Err(format!("{field} must be 0 (for never) or more, got {secs}"));
        }
        i32::try_from(secs).map_err(|_| format!("{field} is too long an interval to store: {secs} seconds"))
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(feature = "server")]
pub use server::execute;
// Test-only: `turn`'s tests read the native tool names.
#[cfg(all(feature = "server", test))]
pub use server::native_tool_definitions;
#[cfg(feature = "server")]
pub use server::tool_definitions;
/// The command line as smelt quotes it back to the model — shared with the
/// idle notice, whose sentence is written over in `turn` (SME-76, SME-79).
#[cfg(feature = "server")]
pub(crate) use pods::shown_command;
