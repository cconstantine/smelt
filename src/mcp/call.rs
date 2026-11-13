//! Calling an MCP tool.

use super::*;
// SME-386 M1's key-shape rule moved to `telemetry` in SME-392, where
// `scrub_urls` and this share it.
use crate::telemetry::looks_like_a_key;

/// How long one tool call may take. A call holds the turn, and the
/// conversation's lock, while it waits (SME-51 B8).
pub(super) const CALL_TIMEOUT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_secs(1)
} else {
    PRODUCT_CALL_TIMEOUT
};

/// The product's call budget, which `CALL_TIMEOUT` is outside a test build.
const PRODUCT_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// `CALL_TIMEOUT` for `config`'s server, on the same rule as
/// `connect_timeout` (SME-356, SME-501): an OAuth server's call may
/// refresh its token at the tests' mock provider first, which under CI's
/// load ran past 1 s ("didn't answer \"echo\" within 1s", CI run
/// 41569971215), so a server with the product's connect budget gets the
/// product's call budget too. A hanging server's tests keep 1 s.
pub(super) fn call_timeout(config: &McpServerConfig) -> std::time::Duration {
    #[cfg(test)]
    if test_support::has_product_budget(config) {
        return PRODUCT_CALL_TIMEOUT;
    }
    let _ = config;
    CALL_TIMEOUT
}

/// [`call_tool_classed`] with no use for the class: the tests' shorthand.
#[cfg(test)]
pub async fn call_tool(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<String, String> {
    call_tool_classed(pool, config, tool_name, arguments, |_, _| Ok(())).await
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
/// Hands the call's class to `check_class` just before it goes out
/// (SME-237): the user's override for this server and tool when one is
/// saved (SME-240, read from Postgres at every call — a save on the
/// settings page takes effect at the next call), and otherwise the class
/// read from the tool list of the very connection the call is sent on,
/// under the same `REGISTRY` guard, so a `tools/list_changed` or an
/// `evict` can't put another list between them. A call that fails before
/// that (arguments that aren't an object, a server that can't be reached,
/// an override row Postgres can't read) is never sent and has no class.
///
/// A `check_class` that says no (SME-241: the conversation's stored rule
/// chain) stops the call *before* `tools/call` is sent: its message is
/// what the model gets and what the `mcp call` span records, so a refused
/// call still shows the class it was refused for and never its input.
pub async fn call_tool_classed(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
    check_class: impl FnOnce(crate::anthropic::tools::ToolClass, &str) -> Result<(), String> + Send,
) -> Result<String, String> {
    let result = call_tool_raw(pool, config, tool_name, arguments, check_class).await?;
    let content = result_text(&result);
    if result.is_error.unwrap_or(false) { Err(content) } else { Ok(content) }
}

/// [`call_tool_classed`]'s call, with the server's result as it came:
/// every content block, `structured_content` and `is_error` (SME-359: a
/// script in the sandbox reads it as data). A result with `is_error` set
/// is `Ok` here, and marks the `mcp call` span failed without its text.
/// An `Err` is smelt's own: the call was refused, or failed below MCP, or
/// the server answered with a JSON-RPC error.
pub async fn call_tool_raw(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
    check_class: impl FnOnce(crate::anthropic::tools::ToolClass, &str) -> Result<(), String> + Send,
) -> Result<rmcp::model::CallToolResult, String> {
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
        match call_tool_unspanned(pool, config, tool_name, arguments, check_class).await {
            Ok(mut result) => {
                if result.is_error.unwrap_or(false) {
                    crate::telemetry::mark_error(&span, CallFailure::REPORTED);
                    // The tool's own error can quote the request, a
                    // header's value included (SME-386 M3).
                    redact_tool_error(pool, config, &mut result).await;
                }
                Ok(result)
            }
            Err(failure) => {
                let message = match failure {
                    CallFailure::Reported(message) => {
                        crate::telemetry::mark_error(&span, CallFailure::REPORTED);
                        message
                    }
                    CallFailure::Failed(message) => {
                        // Redacted as the model's copy is (SME-386 M2):
                        // `mark_error` cuts a URL's query, not its path.
                        crate::telemetry::mark_error(&span, &redact_secrets(config, &message));
                        message
                    }
                    CallFailure::Transport { message, description } => {
                        crate::telemetry::mark_error(&span, &description);
                        message
                    }
                };
                // Read the grant again: a refresh during the call may have
                // replaced the tokens `config` was read with.
                Err(redact_secrets_now(pool, config, &message).await)
            }
        }
    })
    .await
}

/// The name `config`'s server lists the tool a script asked for under
/// (SME-359 review 1, M3): `part` exactly, or, for a model-facing
/// `mcp__…` name (`by_model_name`), the listed tool whose model name it
/// is. `None` when the server lists no such tool: a script calls only
/// listed tools, by their listed names, so a name that merely maps to one
/// (another case, a zero-width character, an embedded `/`) is never sent.
/// An `Err` is a connect failure, its secrets redacted.
pub async fn listed_tool_for_script(
    pool: &sqlx::PgPool,
    config: &McpServerConfig,
    part: &str,
    by_model_name: bool,
) -> Result<Option<String>, String> {
    if let Err(e) = ensure_connected(pool, config, Attempt::SkipRecentFailures).await {
        return Err(redact_secrets_now(pool, config, &e.message).await);
    }
    let registry = REGISTRY.lock().await;
    let Some(conn) = registry.get(&config.id) else {
        return Err(format!("not connected to MCP server {:?}", config.name));
    };
    let wanted = namespaced_tool_name(&config.name, part);
    Ok(conn
        .tools
        .iter()
        .find(|tool| {
            if by_model_name { namespaced_tool_name(&config.name, &tool.name) == wanted } else { tool.name == part }
        })
        .map(|tool| tool.name.to_string()))
}

/// A result's text for the model: `content[].text` blocks joined with
/// newlines; any other block type (image, resource, ...) JSON-encoded
/// inline rather than dropped.
pub fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            McpContentBlock::Text(text) => text.text.clone(),
            other => serde_json::to_string(other).unwrap_or_else(|_| "<unrepresentable MCP content block>".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    check_class: impl FnOnce(crate::anthropic::tools::ToolClass, &str) -> Result<(), String>,
) -> Result<rmcp::model::CallToolResult, CallFailure> {
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
    // The tool by the name its server lists it under: the model calls it
    // by a safe name (SME-365), which for a name like `search.web` isn't
    // that. Read on the connection the call goes out on, and used for the
    // override, the class, the rule check and the request alike.
    let (service, tool_name, (listed, listed_tool)) = REGISTRY
        .lock()
        .await
        .get(&config.id)
        .map(|conn| {
            let real = listed_tool_name(&conn.tools, &config.name, tool_name);
            let listed = listed_class(&conn.tools, &real);
            let tool = conn.tools.iter().find(|tool| tool.name == real).cloned();
            (conn.service.clone(), real, (listed, tool))
        })
        .ok_or_else(|| CallFailure::Failed(format!("not connected to MCP server {:?}", config.name)))?;
    let tool_name = tool_name.as_str();
    // The user's override, read fresh at every call (SME-240), so a save
    // on the settings page takes effect at the next call — no cache, no
    // evict. Read *after* the connect (SME-237 review 1: a failed connect
    // says why, unhid behind a lookup of its own) and *off* the REGISTRY
    // guard (it's a DB query, not connection state). A failure here fails
    // the call before it is sent rather than falling back to the
    // annotations: the override is the user's answer to untrustworthy
    // annotations, and a silent fallback would drop it exactly where
    // SME-241 is about to enforce on this same read. The message names
    // server and tool only — both already on the `mcp call` span — never
    // tool input or output (SME-137).
    let saved = crate::db::get_mcp_tool_class(pool, config.id, tool_name)
        .await
        .map_err(|e| {
            CallFailure::Failed(format!(
                "couldn't read {tool_name:?}'s class override for {:?}: {e}",
                config.name
            ))
        })?;
    // Set for a different tool than the one listed under the name, it can
    // only narrow (SME-295; the rule the offer and the page share).
    let override_class = saved.map(|row| effective_override(&row, listed_tool.as_ref()));
    // The override wins over the connection's list; an unknown text —
    // unreachable under the table's CHECK — is taken as destructive,
    // failing closed toward what SME-241 enforces. The offer path judges
    // by the same function, from the same rows.
    let class = super::judged_class(override_class.as_deref(), listed, &config.name, tool_name);
    // SME-241: the conversation's rule chain speaks here — once the class
    // is known and before the request is built — so a refused call is
    // never sent. Its message is what the model gets and what this span
    // marks failed, and the class the call was refused for is already
    // recorded on the caller's `tool` span.
    if let Err(rule) = check_class(class, tool_name) {
        return Err(CallFailure::Failed(rule));
    }

    let mut request = CallToolRequestParams::new(tool_name.to_string());
    if let Some(arguments) = arguments {
        request = request.with_arguments(arguments);
    }

    let budget = call_timeout(config);
    let result = tokio::time::timeout(budget, service.call_tool(request))
        .await
        .map_err(|_| {
            CallFailure::Failed(format!(
                "MCP server {:?} didn't answer {tool_name:?} within {}s",
                config.name,
                budget.as_secs()
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

    Ok(result)
}

/// An `is_error` result with `config`'s secrets redacted from every
/// string in its content blocks and its `structured_content`, the grant read
/// once for them all.
async fn redact_tool_error(pool: &sqlx::PgPool, config: &McpServerConfig, result: &mut rmcp::model::CallToolResult) {
    let current = current_grant(pool, config).await;
    let redact = |text: &str| {
        let text = redact_secrets(config, text);
        match &current {
            Some(current) => redact_secrets(current, &text),
            None => text,
        }
    };
    for block in &mut result.content {
        match block {
            McpContentBlock::Text(text) => text.text = redact(&text.text),
            // Any other block (an embedded resource, a link) reaches the
            // model serialized and a script whole (review round 1, L2):
            // every string in it, and a block that can't be rebuilt so is
            // replaced by a note.
            other => {
                let rebuilt = serde_json::to_value(&*other).ok().and_then(|mut value| {
                    let mut strings = Vec::new();
                    collect_strings_mut(&mut value, &mut strings);
                    for string in strings {
                        *string = redact(string);
                    }
                    serde_json::from_value::<McpContentBlock>(value).ok()
                });
                *other = rebuilt.unwrap_or_else(|| McpContentBlock::text(format!("{REDACTED} (an MCP content block)")));
            }
        }
    }
    if let Some(structured) = &mut result.structured_content {
        let mut strings = Vec::new();
        collect_strings_mut(structured, &mut strings);
        for string in strings {
            *string = redact(string);
        }
    }
}

/// Every string in `value`, its object keys aside.
fn collect_strings_mut<'a>(value: &'a mut serde_json::Value, into: &mut Vec<&'a mut String>) {
    match value {
        serde_json::Value::String(text) => into.push(text),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|item| collect_strings_mut(item, into)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|item| collect_strings_mut(item, into)),
        _ => {}
    }
}

/// [`redact_secrets`] with the grant as it is stored now, falling back to
/// `config`'s alone when it can't be read.
async fn redact_secrets_now(pool: &sqlx::PgPool, config: &McpServerConfig, text: &str) -> String {
    let current = current_grant(pool, config).await;
    let text = redact_secrets(config, text);
    match current {
        Some(current) => redact_secrets(&current, &text),
        None => text,
    }
}

/// An OAuth server's row as it is stored now (a refresh during the call
/// may have replaced the tokens `config` was read with); `None` for
/// another server, or when it can't be read.
async fn current_grant(pool: &sqlx::PgPool, config: &McpServerConfig) -> Option<McpServerConfig> {
    match config.auth_mode.as_str() {
        "oauth" => crate::db::get_mcp_server_config(pool, config.id).await.ok().flatten(),
        _ => None,
    }
}

/// What stands in for a secret in error text.
pub const REDACTED: &str = "<redacted>";

/// `text` with every secret `config` holds replaced by [`REDACTED`]
/// (SME-359): the URL's query (whole, and each value), password and
/// key-shaped path segments (SME-386),
/// every extra header's value, the OAuth client secret and every token
/// in the stored grant. An error from a call can quote any of them (a
/// connect error quotes the URL, a server's error page the request's
/// headers), and the text reaches the model and a script in the pod.
/// A value under 4 characters is left alone: it can't be told apart
/// from ordinary text, and replacing it would garble the message.
pub fn redact_secrets(config: &McpServerConfig, text: &str) -> String {
    let mut secrets: Vec<String> = Vec::new();
    // The query as configured and as `url` normalizes it (reqwest prints
    // the normalized form), and each value that looks like a key, raw and
    // decoded. Not every value: Exa's built-in URL has
    // `?tools=web_search_exa`, and that tool's name isn't a secret
    // (SME-359's hands-on check found it redacted from every error).
    // A value whose parameter's name says it's a credential goes whatever
    // its shape (review round 1, M1).
    let raw_query = config.url.split_once('?').map(|(_, rest)| rest.split('#').next().unwrap_or(rest).to_string());
    secrets.extend(raw_query.clone());
    if let Some(query) = &raw_query {
        secrets.extend(
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .filter(|(name, value)| names_a_credential(name) || looks_like_a_key(value))
                .map(|(_, value)| value.to_string()),
        );
    }
    if let Ok(url) = url::Url::parse(&config.url) {
        secrets.extend(url.query().map(str::to_string));
        secrets.extend(
            url.query_pairs()
                .filter(|(name, value)| names_a_credential(name) || looks_like_a_key(value))
                .map(|(_, value)| value.into_owned()),
        );
        // A key can be a path segment (`…/mcp/s/<key>/mcp`, SME-386 M1),
        // and reqwest's connect error quotes the path: each segment that
        // looks like a key, as `url` prints it and decoded. The path's
        // words (`mcp`, `s`) stay.
        if let Some(segments) = url.path_segments() {
            for segment in segments.filter(|segment| looks_like_a_key(segment)) {
                secrets.push(segment.to_string());
                if let Ok(decoded) = percent_encoding::percent_decode_str(segment).decode_utf8() {
                    secrets.push(decoded.into_owned());
                }
            }
        }
        secrets.extend(url.password().map(str::to_string));
        // A token can be the URL's user part (`https://ghp_…@host`).
        secrets.extend(Some(url.username().to_string()).filter(|user| !user.is_empty()));
    }
    secrets.extend(config.extra_headers.values().cloned());
    // A header's value can be quoted without its scheme ("Bearer ").
    secrets.extend(config.extra_headers.values().filter_map(|value| value.split_once(' ').map(|(_, rest)| rest.to_string())));
    secrets.extend(config.oauth_client_secret.clone());
    if let Some(credentials) = &config.oauth_credentials {
        collect_tokens(&credentials.0, &mut secrets);
    }
    secrets.retain(|secret| secret.chars().count() >= 4);
    // Longest first, so a value inside a longer one doesn't leave the
    // rest of the longer one behind.
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    secrets.iter().fold(text.to_string(), |text, secret| text.replace(secret.as_str(), REDACTED))
}

/// Whether a query parameter's name says its value is a credential.
fn names_a_credential(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["key", "token", "secret", "auth", "password", "passwd", "pwd", "credential", "signature"].iter().any(|word| name.contains(word))
        || name.starts_with("api")
}

/// Every string under a key that names a token or secret, anywhere in a
/// stored grant (rmcp's `StoredCredentials`).
fn collect_tokens(value: &serde_json::Value, into: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match value {
                    serde_json::Value::String(text) if key.ends_with("_token") || key.ends_with("secret") => {
                        into.push(text.clone())
                    }
                    other => collect_tokens(other, into),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_tokens(item, into)),
        _ => {}
    }
}
