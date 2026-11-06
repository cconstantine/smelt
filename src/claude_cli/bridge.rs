//! The loopback MCP bridge (SME-430): how smelt's tools reach the Claude
//! Code CLI. Claude Code runs every tool itself and has no mode that hands
//! a `tool_use` back to its caller, so each CLI run is given one MCP
//! server, `smelt`, whose tools are that run's request tools. A
//! `tools/call` doesn't run anything here: it is parked
//! (`ParkedCall`) until smelt's own turn loop has run the call, as on
//! every other provider kind, and its next model call answers it.
//!
//! One rmcp streamable-HTTP server on its own `127.0.0.1:0` listener,
//! started the first time a run needs it. Like the preview proxy's
//! listener, it sits outside `request_guard`: nothing but the CLI on this
//! host calls it, and each run has a random path and a bearer token of its
//! own, given to the CLI in its `--mcp-config` only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock as McpContent, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// The MCP server's name in the CLI's config: its tools are
/// `mcp__smelt__<name>` to the model.
pub const SERVER_NAME: &str = "smelt";

/// The `_meta` key Claude Code puts its `tool_use` id under, on every
/// `tools/call` (the planner's probe P2).
pub const TOOL_USE_ID_META: &str = "claudecode/toolUseId";

/// What a tool's name is to the CLI: `mcp__smelt__` and smelt's name, with
/// every character outside `[A-Za-z0-9_-]` made `_` (an MCP tool's own
/// name can hold a `.`; Claude Code's names can't).
pub fn cli_tool_name(name: &str) -> String {
    let plain: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    format!("mcp__{SERVER_NAME}__{plain}")
}

/// One tool call the CLI made, waiting for smelt's result.
#[derive(Debug)]
pub struct ParkedCall {
    /// The model's `tool_use` id, from `_meta` (`TOOL_USE_ID_META`); a
    /// call without one is matched by name and input instead.
    pub tool_use_id: Option<String>,
    /// smelt's name for the tool, mapped back from the CLI's.
    pub name: String,
    pub arguments: Value,
    /// Answers the call. Dropping it answers with an error.
    pub reply: oneshot::Sender<ToolReply>,
}

/// smelt's answer to a parked call: the result's text, and whether the
/// tool failed.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolReply {
    pub text: Vec<String>,
    pub is_error: bool,
}

/// The run's half of the bridge: where the CLI connects, and the calls it
/// makes. Dropping it ends the run's server: the CLI's later requests are
/// refused, and every call still parked is answered with an error.
pub struct RunBridge {
    key: String,
    /// The URL the CLI's `--mcp-config` names.
    pub url: String,
    /// The bearer token the CLI sends, from the same config.
    pub token: String,
    pub calls: mpsc::UnboundedReceiver<ParkedCall>,
    /// The CLI's short name for each tool → smelt's.
    names: HashMap<String, String>,
    bridge: &'static Bridge,
}

impl RunBridge {
    /// smelt's name for the tool the model called `cli_name`
    /// (`mcp__smelt__<name>`), if it's one of this run's.
    pub fn smelt_name(&self, cli_name: &str) -> Option<&str> {
        let short = cli_name.strip_prefix(&format!("mcp__{SERVER_NAME}__"))?;
        self.names.get(short).map(String::as_str)
    }

    /// The `--mcp-config` JSON for this run: one HTTP server, `smelt`.
    pub fn mcp_config(&self) -> Value {
        serde_json::json!({
            "mcpServers": {
                SERVER_NAME: {
                    "type": "http",
                    "url": self.url,
                    "headers": {"Authorization": format!("Bearer {}", self.token)},
                }
            }
        })
    }
}

impl Drop for RunBridge {
    fn drop(&mut self) {
        self.bridge.lock().remove(&self.key);
    }
}

/// One run's server state, held by the bridge's table; its handler holds
/// it weakly, so a run that ended answers nothing.
struct Run {
    token: String,
    tools: Vec<rmcp::model::Tool>,
    /// The CLI's name for each tool → smelt's.
    names: HashMap<String, String>,
    calls: mpsc::UnboundedSender<ParkedCall>,
    service: StreamableHttpService<RunHandler, LocalSessionManager>,
}

/// The bridge: its address, and the runs it serves by path key.
pub struct Bridge {
    addr: std::net::SocketAddr,
    runs: Arc<Runs>,
}

/// The live runs by path key, shared with the listener's handler.
#[derive(Default)]
struct Runs(Mutex<HashMap<String, Arc<Run>>>);

impl Runs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Run>>> {
        // Every change is one insert or remove: nothing is left half-done.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Bridge {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Run>>> {
        self.runs.lock()
    }

    /// Serves a new run whose tools are `tools` (smelt's definitions).
    pub fn register(&'static self, tools: &[crate::anthropic::ToolDefinition]) -> RunBridge {
        let (key, token) = (random_hex(16), random_hex(32));
        let (sender, calls) = mpsc::unbounded_channel();
        let mut names = HashMap::new();
        let mut listed = Vec::new();
        for tool in tools {
            let cli = cli_tool_name(&tool.name);
            let short = cli.trim_start_matches(&format!("mcp__{SERVER_NAME}__")).to_string();
            let schema = match &tool.input_schema {
                Value::Object(map) => map.clone(),
                _ => serde_json::Map::from_iter([("type".to_string(), Value::String("object".to_string()))]),
            };
            listed.push(rmcp::model::Tool::new(short.clone(), tool.description.clone(), Arc::new(schema)));
            names.insert(short, tool.name.clone());
        }
        let run = Arc::new_cyclic(|weak: &Weak<Run>| {
            let handler = RunHandler { run: weak.clone() };
            // Stateless: the CLI speaks MCP's 2026-07-28 revision, which has
            // no sessions, and an older client's requests each stand alone.
            let config = StreamableHttpServerConfig::default().with_legacy_session_mode(false);
            let service = StreamableHttpService::new(move || Ok(handler.clone()), Arc::default(), config);
            Run { token: token.clone(), tools: listed, names: names.clone(), calls: sender, service }
        });
        self.lock().insert(key.clone(), run);
        let url = format!("http://{}/run/{key}", self.addr);
        RunBridge { key, url, token, calls, names, bridge: self }
    }
}

/// The bridge, started the first time it's needed, on a thread and
/// runtime of its own: it outlives whichever task first needed it (a
/// test's runtime ends with its test, and would take a listener spawned
/// on it along).
pub async fn bridge() -> Result<&'static Bridge, String> {
    static BRIDGE: std::sync::OnceLock<Bridge> = std::sync::OnceLock::new();
    static STARTING: Mutex<()> = Mutex::new(());
    if let Some(bridge) = BRIDGE.get() {
        return Ok(bridge);
    }
    let _starting = STARTING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(bridge) = BRIDGE.get() {
        return Ok(bridge);
    }
    let failed = |e: std::io::Error| format!("couldn't start smelt's tool bridge for Claude Code: {e}");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(failed)?;
    listener.set_nonblocking(true).map_err(failed)?;
    let addr = listener.local_addr().map_err(failed)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("claude-cli-bridge")
        .enable_all()
        .build()
        .map_err(failed)?;
    let runs = Arc::new(Runs::default());
    let app = axum::Router::new().fallback(route).with_state(runs.clone());
    std::thread::Builder::new()
        .name("claude-cli-bridge".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                let served = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => axum::serve(listener, app).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = served {
                    tracing::error!("smelt's tool bridge for Claude Code stopped: {e}");
                }
            });
        })
        .map_err(failed)?;
    Ok(BRIDGE.get_or_init(|| Bridge { addr, runs }))
}

/// Hands a request to its run's MCP service, once its path and bearer
/// token match a live run; anything else is a 404.
async fn route(axum::extract::State(runs): axum::extract::State<Arc<Runs>>, request: axum::extract::Request) -> axum::response::Response {
    use axum::response::IntoResponse;
    let not_found = || axum::http::StatusCode::NOT_FOUND.into_response();
    let Some(key) = request.uri().path().strip_prefix("/run/").map(str::to_string) else {
        return not_found();
    };
    let Some(run) = runs.lock().get(&key).cloned() else {
        return not_found();
    };
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !same_secret(presented.as_bytes(), run.token.as_bytes()) {
        return not_found();
    }
    let response = run.service.handle(request).await;
    response.map(axum::body::Body::new)
}

/// Compares two secrets in time that doesn't depend on where they differ.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `bytes` random bytes, as hex.
pub(crate) fn random_hex(bytes: usize) -> String {
    use ssh_key::rand_core::{OsRng, RngCore};
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// The MCP server one run's CLI talks to.
#[derive(Clone)]
struct RunHandler {
    run: Weak<Run>,
}

/// What a call gets when its run is gone, or smelt dropped it unanswered.
pub const ENDED: &str = "smelt ended this tool call before it had a result.";

impl ServerHandler for RunHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.run.upgrade().map(|run| run.tools.clone()).unwrap_or_default();
        Ok(ListToolsResult { tools, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let failed = |text: String| Ok(CallToolResult::error(vec![McpContent::text(text)]).into());
        let Some(run) = self.run.upgrade() else {
            return failed(ENDED.to_string());
        };
        let Some(name) = run.names.get(request.name.as_ref()).cloned() else {
            return failed(format!("smelt has no tool named {}.", request.name));
        };
        let id_in = |meta: Option<&rmcp::model::RequestMetaObject>| {
            meta?.get(TOOL_USE_ID_META)?.as_str().map(str::to_string)
        };
        let tool_use_id = id_in(request.meta.as_ref()).or_else(|| id_in(Some(&context.meta)));
        let arguments = Value::Object(request.arguments.clone().unwrap_or_default());
        let (reply, answer) = oneshot::channel();
        if run.calls.send(ParkedCall { tool_use_id, name, arguments, reply }).is_err() {
            return failed(ENDED.to_string());
        }
        // Not held while parked: a run that ends drops its receiver and
        // with it this call's sender.
        drop(run);
        match answer.await {
            Ok(ToolReply { text, is_error }) => {
                let content = text.into_iter().map(McpContent::text).collect();
                Ok(if is_error { CallToolResult::error(content) } else { CallToolResult::success(content) }.into())
            }
            Err(_) => failed(ENDED.to_string()),
        }
    }
}
