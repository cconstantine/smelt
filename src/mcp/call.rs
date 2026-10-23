//! Calling an MCP tool.

use super::*;

/// How long one tool call may take. A call holds the turn, and the
/// conversation's lock, while it waits (SME-51 B8).
pub(super) const CALL_TIMEOUT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(1)
} else {
    std::time::Duration::from_secs(120)
};

/// [`call_tool_classed`] with no use for the class: the tests' shorthand.
#[cfg(test)]
pub async fn call_tool(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<String, String> {
    call_tool_classed(pool, config, tool_name, arguments, |_| {}).await
}

/// Dispatches a `tools/call` to `config`'s server and turns the result
/// into a plain string — `content[].text` blocks concatenated; any other
/// content block type (image, resource, ...) is JSON-encoded inline rather
/// than dropped, matching the same "don't lose it" fallback other tool
/// dispatch paths in this codebase already use. An `is_error` result comes
/// back as `Err` so `anthropic::tools::execute` wraps it in a
/// `ContentBlock::ToolResult { is_error: Some(true), .. }` like any other
/// failed tool call.
///
/// Hands the call's class to `on_class` just before it goes out
/// (SME-237): read from the tool list of the very connection the
/// call is sent on, under the same `REGISTRY` guard, so a `tools/list_changed`
/// or an `evict` can't put another list between them. A call that fails
/// before that (arguments that aren't an object, a server that can't be
/// reached) is never sent and has no class.
pub async fn call_tool_classed(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
    on_class: impl FnOnce(crate::anthropic::tools::ToolClass) + Send,
) -> Result<String, String> {
    use tracing::field::Empty;
    // The server and tool, never the arguments or result (SME-137).
    let span = tracing::info_span!(
        "mcp call",
        smelt.mcp.server = %config.name,
        smelt.mcp.tool = %tool_name,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        call_tool_unspanned(pool, config, tool_name, arguments, on_class).await.map_err(|e| match e {
            CallFailure::Reported(message) => {
                crate::telemetry::mark_error(&span, CallFailure::REPORTED);
                message
            }
            CallFailure::Failed(message) => {
                crate::telemetry::mark_error(&span, &message);
                message
            }
            CallFailure::Transport { message, description } => {
                crate::telemetry::mark_error(&span, &description);
                message
            }
        })
    })
    .await
}

/// How a `tools/call` failed: the tool said so, in output of its own (or
/// smelt refused the call's input, quoting it), or the call itself did.
/// Only the second's message goes on the span: a span never holds tool
/// output or input (SME-137 review 1).
enum CallFailure {
    Reported(String),
    Failed(String),
    /// Below MCP: the span gets `description`, the model `message`.
    Transport { message: String, description: String },
}

/// What a `tools/call` that failed below MCP says on its span: the tool,
/// the server and the kind of failure, never the error's text, which can
/// quote the server's reply (SME-137 review 2).
pub(super) fn transport_failure_description(tool_name: &str, server: &str, error: &rmcp::service::ServiceError) -> String {
    use rmcp::service::ServiceError;
    let kind = match error {
        ServiceError::McpError(_) => "the server returned an error",
        ServiceError::TransportSend(_) => "a transport error (the server's reply is in the conversation)",
        ServiceError::TransportClosed => "the transport closed",
        ServiceError::UnexpectedResponse => "an unexpected response",
        ServiceError::Timeout { .. } => "rmcp's request timeout",
        ServiceError::Cancelled { .. } => "the request was cancelled",
        _ => "an rmcp error",
    };
    format!("MCP tool call to {tool_name:?} on {server:?} failed: {kind}")
}

impl CallFailure {
    const REPORTED: &'static str = "the MCP tool reported an error (its output is in the conversation)";
}

async fn call_tool_unspanned(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
    on_class: impl FnOnce(crate::anthropic::tools::ToolClass),
) -> Result<String, CallFailure> {
    let arguments = match arguments {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => None,
        other => {
            return Err(CallFailure::Reported(format!(
                "tool arguments must be a JSON object, got: {other}"
            )));
        }
    };

    ensure_connected(pool, config, Attempt::SkipRecentFailures)
        .await
        .map_err(|e| CallFailure::Failed(e.message))?;
    let (service, class) = REGISTRY
        .lock()
        .await
        .get(&config.id)
        .map(|conn| (conn.service.clone(), listed_class(&conn.tools, tool_name)))
        .ok_or_else(|| CallFailure::Failed(format!("not connected to MCP server {:?}", config.name)))?;
    on_class(class);

    let mut request = CallToolRequestParams::new(tool_name.to_string());
    if let Some(arguments) = arguments {
        request = request.with_arguments(arguments);
    }

    let result = tokio::time::timeout(CALL_TIMEOUT, service.call_tool(request))
        .await
        .map_err(|_| {
            CallFailure::Failed(format!(
                "MCP server {:?} didn't answer {tool_name:?} within {}s",
                config.name,
                CALL_TIMEOUT.as_secs()
            ))
        })?;
    let result = match result {
        Ok(result) => result,
        Err(e) => {
            // Below MCP (a transport error, such as a 401 that survived
            // `AuthClient`'s refresh and retry, or a closed connection),
            // the connection is taken as dead, so the next caller connects
            // again rather than every call failing on it (SME-113). A
            // JSON-RPC error from the tool itself isn't the connection's.
            let from_the_tool = matches!(e, rmcp::service::ServiceError::McpError(_));
            if !from_the_tool {
                drop_connection(config.id, &service).await;
            }
            let message = format!("MCP tool call to {:?} on {:?} failed: {e}", tool_name, config.name);
            // The server's own error text, or its reply quoted in a
            // transport error, can quote the call's input (review 2).
            return Err(if from_the_tool {
                CallFailure::Reported(message)
            } else {
                CallFailure::Transport { description: transport_failure_description(tool_name, &config.name, &e), message }
            });
        }
    };

    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            McpContentBlock::Text(text) => text.text,
            other => serde_json::to_string(&other)
                .unwrap_or_else(|_| "<unrepresentable MCP content block>".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_error.unwrap_or(false) {
        Err(CallFailure::Reported(content))
    } else {
        Ok(content)
    }
}
