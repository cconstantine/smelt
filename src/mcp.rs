//! Thin wrapper around [`rmcp`](https://crates.io/crates/rmcp) (the
//! official Rust MCP SDK) for smelt's externally-configured MCP servers —
//! see `docs/projects/plans/mcp-servers.md`. `rmcp` owns the actual wire
//! protocol (JSON-RPC framing, Streamable HTTP's two response modes,
//! session tracking, notification dispatch); this module owns the
//! connection registry keyed by `mcp_servers.id`, the
//! `mcp__<server_name>__<tool_name>` naming translation, smelt's
//! `ClientHandler` for server-initiated notifications, and turning an MCP
//! tool-call result into a smelt `ToolResult` string.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolRequestParams, ContentBlock as McpContentBlock, Tool as McpTool};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio::sync::Mutex as AsyncMutex;

use crate::anthropic::ToolDefinition;
use crate::db::McpServerConfig;

const TOOL_NAME_PREFIX: &str = "mcp__";
const TOOL_NAME_SEPARATOR: &str = "__";

fn namespaced_tool_name(server_name: &str, tool_name: &str) -> String {
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

/// Parses `extra_headers` into the `http` crate's typed header map
/// `rmcp`'s transport config expects. A per-server config error (an
/// invalid header name/value the UI let through) surfaces as a plain
/// `Err` rather than a panic — see `docs/projects/plans/mcp-servers.md`'s
/// "Managing servers."
fn build_header_map(
    extra_headers: &HashMap<String, String>,
) -> Result<HashMap<http::HeaderName, http::HeaderValue>, String> {
    extra_headers
        .iter()
        .map(|(name, value)| {
            let header_name = http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| format!("invalid header name {name:?}: {e}"))?;
            let header_value = http::HeaderValue::from_str(value)
                .map_err(|e| format!("invalid header value for {name:?}: {e}"))?;
            Ok((header_name, header_value))
        })
        .collect()
}

/// smelt's `ClientHandler` implementation — the extension point `rmcp`
/// calls into for server-initiated notifications. Only `tools/list_changed`
/// drives real behavior in this pass: it marks the connection's cached
/// tool list stale so the next call to `tool_definitions_for` refreshes it,
/// rather than waiting for someone to edit the server's config. Everything
/// else `ClientHandler` can report (progress, resource/prompt list
/// changes, logging messages, ...) uses the trait's own no-op defaults —
/// accepted, not acted on; see the plan's "Server-initiated notifications."
#[derive(Clone)]
struct SmeltClientHandler {
    stale: Arc<AtomicBool>,
}

impl ClientHandler for SmeltClientHandler {
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
        self.stale.store(true, Ordering::SeqCst);
        std::future::ready(())
    }
}

struct Connection {
    service: RunningService<RoleClient, SmeltClientHandler>,
    tools: Vec<McpTool>,
    stale: Arc<AtomicBool>,
}

/// One entry per configured server (`mcp_servers.id`), lazily connected on
/// first use. Not persisted — re-derivable at any time via a fresh
/// `initialize` handshake, the same reasoning `sandbox.rs`'s per-pod
/// connection registry uses. `update_mcp_server`/`delete_mcp_server`
/// (`src/api/mcp.rs`) call `evict` to drop a row's stale entry when its
/// config changes.
static REGISTRY: LazyLock<AsyncMutex<HashMap<i64, Connection>>> =
    LazyLock::new(|| AsyncMutex::new(HashMap::new()));

/// Drops a server's cached connection, if any — called after an edit or
/// delete so a stale connection (old URL, old headers) is never reused.
/// A future `tool_definitions_for`/`call_tool` call reconnects fresh.
pub async fn evict(server_id: i64) {
    REGISTRY.lock().await.remove(&server_id);
}

/// Calls `f` and, if it fails, retries exactly once. A server's very first
/// connection attempt is the one most likely to hit a one-off transient
/// failure (a slow DNS lookup, a dropped packet, a dev-server rebuild
/// racing the request) — see `docs/projects/plans/mcp-servers.md`'s
/// "Open questions." Generic over `f`'s return type so it's unit-testable
/// without a real network call; `connect`'s only caller wires it in below.
async fn retry_once<F, Fut, T>(mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    match f().await {
        Ok(value) => Ok(value),
        Err(_first_error) => f().await,
    }
}

async fn connect(config: &McpServerConfig) -> Result<Connection, String> {
    let headers = build_header_map(&config.extra_headers.0)?;
    let transport_config =
        StreamableHttpClientTransportConfig::with_uri(config.url.clone()).custom_headers(headers);
    let transport = StreamableHttpClientTransport::from_config(transport_config);

    let stale = Arc::new(AtomicBool::new(false));
    let handler = SmeltClientHandler { stale: stale.clone() };
    let service = handler
        .serve(transport)
        .await
        .map_err(|e| format!("failed to connect to MCP server {:?}: {e}", config.name))?;

    let tools = service
        .list_all_tools()
        .await
        .map_err(|e| format!("failed to list tools from MCP server {:?}: {e}", config.name))?;

    Ok(Connection { service, tools, stale })
}

/// Ensures `REGISTRY` has a live, up-to-date entry for `config.id` —
/// connecting for the first time, or just re-listing tools if the existing
/// connection's cached list went stale (a `tools/list_changed`
/// notification fired). A `list_all_tools` failure on an existing
/// connection is treated as "the connection is broken," not just "the list
/// is stale": the entry is dropped so the fallback below reconnects fully.
async fn ensure_connected(config: &McpServerConfig) -> Result<(), String> {
    let mut registry = REGISTRY.lock().await;

    if let Some(conn) = registry.get_mut(&config.id) {
        if conn.stale.swap(false, Ordering::SeqCst) {
            match conn.service.list_all_tools().await {
                Ok(tools) => conn.tools = tools,
                Err(_) => {
                    registry.remove(&config.id);
                }
            }
        }
        if registry.contains_key(&config.id) {
            return Ok(());
        }
    }

    let connection = retry_once(|| connect(config)).await?;
    registry.insert(config.id, connection);
    Ok(())
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
pub async fn tool_definitions_for(configs: &[McpServerConfig]) -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();
    for config in configs {
        if let Err(e) = ensure_connected(config).await {
            tracing::warn!(server = %config.name, error = %e, "MCP server unreachable this turn; its tools are unavailable");
            continue;
        }
        let registry = REGISTRY.lock().await;
        if let Some(conn) = registry.get(&config.id) {
            definitions.extend(conn.tools.iter().map(|tool| mcp_tool_to_definition(&config.name, tool)));
        }
    }
    definitions
}

/// Attempts a real connection to `config`'s server and reports its current
/// tool names on success — the `/mcp-servers` UI's live "connected" status
/// (index page's badge, edit page's full status) rather than a cached
/// guess, since a stale "looks fine" indicator would be worse than no
/// indicator at all. Shares `ensure_connected`'s registry/retry logic with
/// `tool_definitions_for`/`call_tool`, so a server already connected this
/// turn is reported without a second round-trip.
pub async fn connection_check(config: &McpServerConfig) -> Result<Vec<String>, String> {
    ensure_connected(config).await?;
    let registry = REGISTRY.lock().await;
    let conn = registry
        .get(&config.id)
        .ok_or_else(|| format!("connection to MCP server {:?} vanished immediately after connecting", config.name))?;
    Ok(conn.tools.iter().map(|tool| tool.name.to_string()).collect())
}

/// Dispatches a `tools/call` to `config`'s server and turns the result
/// into a plain string — `content[].text` blocks concatenated; any other
/// content block type (image, resource, ...) is JSON-encoded inline rather
/// than dropped, matching the same "don't lose it" fallback other tool
/// dispatch paths in this codebase already use. An `is_error` result comes
/// back as `Err` so `anthropic::tools::execute` wraps it in a
/// `ContentBlock::ToolResult { is_error: Some(true), .. }` like any other
/// failed tool call.
pub async fn call_tool(config: &McpServerConfig, tool_name: &str, arguments: serde_json::Value) -> Result<String, String> {
    let arguments = match arguments {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => None,
        other => return Err(format!("tool arguments must be a JSON object, got: {other}")),
    };

    ensure_connected(config).await?;
    let registry = REGISTRY.lock().await;
    let conn = registry
        .get(&config.id)
        .ok_or_else(|| format!("not connected to MCP server {:?}", config.name))?;

    let mut request = CallToolRequestParams::new(tool_name.to_string());
    if let Some(arguments) = arguments {
        request = request.with_arguments(arguments);
    }

    let result = conn
        .service
        .call_tool(request)
        .await
        .map_err(|e| format!("MCP tool call to {:?} on {:?} failed: {e}", tool_name, config.name))?;

    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            McpContentBlock::Text(text) => text.text,
            other => serde_json::to_string(&other).unwrap_or_else(|_| "<unrepresentable MCP content block>".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_error.unwrap_or(false) {
        Err(content)
    } else {
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
    use rmcp::handler::server::tool::ToolCallContext;
    use rmcp::model::{CallToolResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo};
    use rmcp::service::RequestContext;
    use rmcp::{ErrorData, RoleServer, ServerHandler};
    use tokio::sync::RwLock;

    /// A minimal MCP server for tests: exposes whatever tools the test
    /// registers, echoes its input back as the call result, and can be
    /// told (via `disable`/`enable`) to change its tool list and fire a
    /// real `tools/list_changed` notification — exactly the shape
    /// `rmcp`'s own `test_tool_disable_notification.rs` integration test
    /// uses, since that's the authoritative example of this pattern.
    #[derive(Clone)]
    struct MockMcpServer {
        router: Arc<RwLock<ToolRouter<Self>>>,
    }

    impl MockMcpServer {
        /// `second_tool_disabled: true` registers a second tool but keeps
        /// it hidden from `list_tools` until something calls
        /// `enable_route("second_tool")` — the only thing that actually
        /// fires a `tools/list_changed` notification (`add_route` alone
        /// does not; see `ToolRouter::disable_route`/`enable_route` in
        /// `rmcp`'s source). Used by the list-changed test to exercise a
        /// real notification, not just a tool being added.
        fn new(second_tool_disabled: bool) -> Self {
            let mut router = ToolRouter::<Self>::new();
            router.add_route(ToolRoute::new_dyn(
                rmcp::model::Tool::new(
                    "echo",
                    "Echoes its input back",
                    Arc::new(serde_json::from_value(serde_json::json!({"type": "object"})).unwrap()),
                ),
                |ctx| {
                    Box::pin(async move {
                        let args = ctx.arguments.clone().unwrap_or_default();
                        Ok(CallToolResult::success(vec![McpContentBlock::Text(rmcp::model::TextContent::new(
                            serde_json::to_string(&args).unwrap_or_default(),
                        ))])
                        .into())
                    })
                },
            ));
            if second_tool_disabled {
                router.add_route(ToolRoute::new_dyn(
                    rmcp::model::Tool::new(
                        "second_tool",
                        "A second tool, initially hidden",
                        Arc::new(serde_json::from_value(serde_json::json!({"type": "object"})).unwrap()),
                    ),
                    |_ctx| Box::pin(async { Ok(CallToolResult::default().into()) }),
                ));
                router.disable_route("second_tool");
            }
            Self { router: Arc::new(RwLock::new(router)) }
        }
    }

    impl ServerHandler for MockMcpServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<rmcp::model::CallToolResponse, ErrorData> {
            let router = self.router.read().await;
            router.call(ToolCallContext::new(self, request, context)).await
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            let router = self.router.read().await;
            Ok(ListToolsResult { tools: router.list_all(), ..Default::default() })
        }

        fn on_initialized(
            &self,
            context: NotificationContext<RoleServer>,
        ) -> impl std::future::Future<Output = ()> + rmcp::service::MaybeSendFuture + '_ {
            let router = self.router.clone();
            let peer = context.peer.clone();
            async move {
                router.write().await.bind_peer_notifier(&peer);
            }
        }
    }

    /// Connects a fresh in-process client/server pair over a
    /// `tokio::io::duplex` transport (the same transport rmcp's own
    /// notification test uses) — this module's tests exercise the
    /// registry/dispatch/namespacing logic in `mcp.rs`, not the Streamable
    /// HTTP wire format itself (that's `rmcp`'s own tested responsibility;
    /// see `docs/projects/plans/mcp-servers.md`'s "Proving this works").
    /// Registers the resulting connection under `server_id` in the shared
    /// `REGISTRY`, bypassing `connect()`'s HTTP-specific transport
    /// construction.
    async fn register_test_connection(server_id: i64, second_tool_disabled: bool) -> MockMcpServer {
        let server = MockMcpServer::new(second_tool_disabled);
        let (server_transport, client_transport) = tokio::io::duplex(4096);

        let server_for_task = server.clone();
        // `serve()` returns (a `RunningService`) once the handshake
        // completes — it does not block for the connection's whole
        // lifetime. Its `RunningService::drop` cancels the connection (see
        // rmcp's own doc comment on that impl), so the spawned task's
        // return value must never be collected/dropped for as long as this
        // test needs the connection alive. Deliberately leaking the
        // `JoinHandle` (not just discarding it — dropping an *unawaited*
        // `JoinHandle` still drops the task's output as soon as the task
        // completes) is what rmcp's own `test_tool_disable_notification.rs`
        // achieves by holding the handle for the whole test and only
        // `.abort()`ing it at the very end; `mem::forget` gets the same
        // "never dropped" effect without threading the handle through
        // every test.
        let server_handle = tokio::spawn(async move { server_for_task.serve(server_transport).await });
        std::mem::forget(server_handle);

        let stale = Arc::new(AtomicBool::new(false));
        let handler = SmeltClientHandler { stale: stale.clone() };
        let service = handler.serve(client_transport).await.expect("client should connect");
        let tools = service.list_all_tools().await.expect("list_all_tools should succeed");

        REGISTRY.lock().await.insert(server_id, Connection { service, tools, stale });
        server
    }

    fn test_config(id: i64, name: &str) -> McpServerConfig {
        McpServerConfig {
            id,
            name: name.to_string(),
            url: "http://unused.invalid".to_string(),
            extra_headers: sqlx::types::Json(HashMap::new()),
            created_at: Default::default(),
            updated_at: Default::default(),
        }
    }

    #[test]
    fn test_parse_tool_name_splits_server_and_tool() {
        assert_eq!(parse_tool_name("mcp__github__list_issues"), Some(("github", "list_issues")));
        // A tool name that itself contains further underscores still
        // resolves — only the first `__` after the server name matters.
        assert_eq!(parse_tool_name("mcp__github__list__issues"), Some(("github", "list__issues")));
        assert_eq!(parse_tool_name("read_file"), None, "native tool names must not be misparsed as MCP-dispatched");
        assert_eq!(parse_tool_name("mcp__no_separator"), None);
    }

    #[test]
    fn test_build_header_map_rejects_invalid_header_values() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer ok".to_string());
        assert!(build_header_map(&headers).is_ok());

        let mut invalid = HashMap::new();
        // A raw newline is not a legal header value.
        invalid.insert("Authorization".to_string(), "Bearer bad\nvalue".to_string());
        assert!(build_header_map(&invalid).is_err());
    }

    #[tokio::test]
    async fn test_tool_definitions_for_merges_and_namespaces_tools_from_connected_servers() {
        let server_id = -1001;
        register_test_connection(server_id, false).await;

        let definitions = tool_definitions_for(&[test_config(server_id, "test-server")]).await;

        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "mcp__test-server__echo");
        assert_eq!(definitions[0].description, "Echoes its input back");

        REGISTRY.lock().await.remove(&server_id);
    }

    #[tokio::test]
    async fn test_call_tool_dispatches_and_returns_text_content() {
        let server_id = -1002;
        register_test_connection(server_id, false).await;

        let result = call_tool(&test_config(server_id, "test-server"), "echo", serde_json::json!({"hello": "world"}))
            .await
            .expect("call_tool should succeed");

        assert!(result.contains("hello"), "expected echoed arguments in result, got: {result}");
        assert!(result.contains("world"));

        REGISTRY.lock().await.remove(&server_id);
    }

    #[tokio::test]
    async fn test_tool_list_changed_notification_refreshes_cached_tools() {
        let server_id = -1003;
        // `second_tool_disabled: true` — the second tool exists on the
        // server from the start but is hidden, so `enable_route` below is
        // what actually fires `tools/list_changed` (adding a brand-new
        // route via `add_route` does not notify at all — confirmed against
        // `rmcp`'s source: only `enable_route`/`disable_route` call
        // `notify_if_visible`).
        let server = register_test_connection(server_id, true).await;

        let before = tool_definitions_for(&[test_config(server_id, "test-server")]).await;
        assert_eq!(before.len(), 1, "starts with just the echo tool; second_tool begins disabled");

        // Real behavior, not a manual eviction: nothing but the
        // notification tells tool_definitions_for the cached list is stale.
        server.router.write().await.enable_route("second_tool");

        // Give the notification a moment to propagate over the duplex
        // transport and flip the connection's `stale` flag.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let after = tool_definitions_for(&[test_config(server_id, "test-server")]).await;
        let names: Vec<&str> = after.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(after.len(), 2, "expected both tools after the list_changed notification, got: {names:?}");
        assert!(names.contains(&"mcp__test-server__second_tool"));

        REGISTRY.lock().await.remove(&server_id);
    }

    #[tokio::test]
    async fn test_connection_check_reports_tool_names_for_a_connected_server() {
        let server_id = -1004;
        register_test_connection(server_id, false).await;

        let tool_names = connection_check(&test_config(server_id, "test-server"))
            .await
            .expect("a registered connection should report as connected");

        assert_eq!(tool_names, vec!["echo".to_string()]);

        REGISTRY.lock().await.remove(&server_id);
    }

    #[tokio::test]
    async fn test_connection_check_errors_for_an_unconfigured_server() {
        // No `register_test_connection` call — nothing in the registry, and
        // `test_config`'s URL is deliberately unroutable, so this must
        // report an error rather than panicking or hanging.
        let server_id = -1005;
        let result = connection_check(&test_config(server_id, "unreachable-server")).await;
        assert!(result.is_err(), "expected an unreachable server to report Err, got {result:?}");
    }

    #[tokio::test]
    async fn test_retry_once_succeeds_after_a_single_failure() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let result = retry_once(|| {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move { if n == 0 { Err("first attempt fails".to_string()) } else { Ok(42) } }
        })
        .await;

        assert_eq!(result, Ok(42));
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "expected exactly one retry after the first failure");
    }

    #[tokio::test]
    async fn test_retry_once_gives_up_after_a_second_failure() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let result: Result<i32, String> = retry_once(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Err("still failing".to_string()) }
        })
        .await;

        assert_eq!(result, Err("still failing".to_string()));
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "expected no more than one retry");
    }
}
