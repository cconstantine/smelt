use super::*;

use std::sync::atomic::AtomicU32;

use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
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
    /// Every `tools/call` this server has been asked for (SME-241):
    /// a refused call proves the refusal happened by this staying 0.
    calls: Arc<AtomicU32>,
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
                Arc::new(
                    serde_json::from_value(serde_json::json!({"type": "object"})).unwrap(),
                ),
            ),
            |ctx| {
                Box::pin(async move {
                    let args = ctx.arguments.clone().unwrap_or_default();
                    Ok(CallToolResult::success(vec![McpContentBlock::Text(
                        rmcp::model::TextContent::new(
                            serde_json::to_string(&args).unwrap_or_default(),
                        ),
                    )])
                    .into())
                })
            },
        ));
        if second_tool_disabled {
            router.add_route(ToolRoute::new_dyn(
                rmcp::model::Tool::new(
                    "second_tool",
                    "A second tool, initially hidden",
                    Arc::new(
                        serde_json::from_value(serde_json::json!({"type": "object"})).unwrap(),
                    ),
                )
                // Read-only, for the classes tests (SME-237).
                .annotate(rmcp::model::ToolAnnotations::new().read_only(true)),
                |_ctx| Box::pin(async { Ok(CallToolResult::default().into()) }),
            ));
            router.disable_route("second_tool");
        }
        Self {
            router: Arc::new(RwLock::new(router)),
            calls: Arc::new(AtomicU32::new(0)),
        }
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        // An unlisted tool that never answers, for the call timeout.
        if request.name == "hang" {
            std::future::pending::<()>().await;
        }
        // An unlisted tool that reports an error with output of its own.
        if request.name == "fail" {
            return Ok(CallToolResult::error(vec![McpContentBlock::Text(rmcp::model::TextContent::new(
                "SECRET-OUTPUT of the failed tool".to_string(),
            ))])
            .into());
        }
        let router = self.router.read().await;
        router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let router = self.router.read().await;
        Ok(ListToolsResult {
            tools: router.list_all(),
            ..Default::default()
        })
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
/// registry/dispatch/namespacing logic in `mcp/`, not the Streamable
/// HTTP wire format itself (that's `rmcp`'s own tested responsibility;
/// the real HTTP path was proven separately, live, against GitHub's
/// hosted MCP server — see SME-15).
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
    let server_handle =
        tokio::spawn(async move { server_for_task.serve(server_transport).await });
    std::mem::forget(server_handle);

    let stale = Arc::new(AtomicBool::new(false));
    let handler = SmeltClientHandler {
        stale: stale.clone(),
    };
    let service = handler
        .serve(client_transport)
        .await
        .expect("client should connect");
    let tools = service
        .list_all_tools()
        .await
        .expect("list_all_tools should succeed");

    REGISTRY.lock().await.insert(
        server_id,
        Connection {
            service: Arc::new(service),
            tools,
            stale,
        },
    );
    server
}

/// Never actually connected to — every test below either pre-populates
/// `REGISTRY` directly (so `connect`, which reads the server's row, is
/// never reached) and reads nothing else from the pool. Tests that
/// connect, and any test that calls a tool (since SME-240 the call
/// reads the class-override row), take a real `#[sqlx::test]` pool;
/// connecting ones also a row (`create_static_server`). Same
/// lazy-fake-pool pattern `anthropic::tools`' own tests already use for
/// the same reason. OAuth-touching behavior gets its own
/// `#[sqlx::test]`s in `mcp_oauth.rs`.
fn test_pool() -> sqlx::PgPool {
    sqlx::PgPool::connect_lazy("postgres://unused:unused@localhost/unused")
        .expect("lazy pool construction never fails")
}

fn test_config(id: i64, name: &str) -> McpServerConfig {
    McpServerConfig {
        id,
        name: name.to_string(),
        url: "http://unused.invalid".to_string(),
        extra_headers: sqlx::types::Json(HashMap::new()),
        auth_mode: "static_headers".to_string(),
        oauth_credentials: None,
        oauth_client_id: None,
        oauth_client_secret: None,
        created_at: Default::default(),
        updated_at: Default::default(),
    }
}

/// The chain a test that rules nothing passes to `tool_definitions_for`:
/// allow-all, what a conversation with no `agents` row gets.
fn unruled() -> crate::personas::rules::RuleChain {
    crate::personas::rules::RuleChain::default()
}

/// A conversation with an `active` agent row whose stored `rule_chain` is
/// `entries` (SME-241): the stand-in for a started agent, so the offer and
/// the call both read real rules from the database.
async fn agent_conversation(pool: &sqlx::PgPool, entries: serde_json::Value) -> i64 {
    let id = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title) VALUES ('ruled') RETURNING id")
        .fetch_one(pool)
        .await
        .expect("a conversation");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, rule_chain) VALUES ($1, $1, 1, 'b', 'active', $2)",
    )
    .bind(id)
    .bind(entries)
    .execute(pool)
    .await
    .expect("an agent row");
    id
}

/// Makes the next row `pool`'s database inserts into `mcp_servers` take
/// an id no other test in the process uses, and returns it: `REGISTRY`
/// and the other maps are shared by every test in the process, while
/// each `#[sqlx::test]` database numbers rows from 1.
async fn next_server_id(pool: &sqlx::PgPool) -> i64 {
    static NEXT_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(20_000_000);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    sqlx::query(&format!("ALTER TABLE mcp_servers ALTER COLUMN id RESTART WITH {id}"))
        .execute(pool)
        .await
        .expect("set the next mcp_servers id");
    id
}

/// A `static_headers` server row at `url`, with an id of its own: since
/// SME-167 a connect reads its server's row, so a test that reaches
/// `connect` needs one.
async fn create_static_server(pool: &sqlx::PgPool, url: &str) -> McpServerConfig {
    let id = next_server_id(pool).await;
    crate::db::create_mcp_server_config(pool, &format!("static-{id}"), url, &HashMap::new(), "static_headers", None, None)
        .await
        .expect("create mcp server config")
}

/// SME-113: an MCP server over real HTTP that checks bearer tokens, and
/// the OAuth provider that issues them, in one Axum app. The MCP side
/// answers `initialize`, `tools/list` and `tools/call` with plain JSON
/// (Streamable HTTP's JSON response mode) and 401s, with a
/// `WWW-Authenticate` challenge as GitHub and Linear send, any token it
/// doesn't currently accept. The provider side is rmcp's legacy
/// endpoints at the root (`/token`, `/register`; no metadata), and
/// rotates refresh tokens the way GitHub does: a refresh kills the old
/// refresh token and every access token issued before it.
mod oauth_http {
    use super::*;

    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicU32;

    use axum::extract::{Form, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;

    #[derive(Default)]
    struct Tokens {
        accepted: HashSet<String>,
        live_refresh: HashSet<String>,
        issued: u32,
    }

    pub struct Mock {
        pub mcp_url: String,
        tokens: StdMutex<Tokens>,
        /// `grant_type=refresh_token` requests answered with a token.
        pub refreshes: AtomicU32,
        /// `initialize` requests that got past the token check.
        pub initializes: AtomicU32,
        registered: AtomicU32,
        /// Answer every refresh with `invalid_grant`, as a provider
        /// does for a revoked or expired refresh token.
        pub reject_refreshes: AtomicBool,
        /// Refuse refreshes the way GitHub does: HTTP 200 with
        /// `{"error": "bad_refresh_token"}` in the body.
        pub reject_refreshes_like_github: AtomicBool,
        /// A pre-registered client's `(client_id, client_secret)`: a
        /// refresh that doesn't authenticate with it is refused, as
        /// GitHub refuses one without `client_secret`.
        pub required_secret: StdMutex<Option<(String, String)>>,
        /// The `client_id` each refresh request named, recorded as it
        /// arrives (before `refresh_gate`).
        pub refresh_client_ids: StdMutex<Vec<String>>,
        /// When set, a refresh waits for a permit before answering.
        pub refresh_gate: StdMutex<Option<Arc<tokio::sync::Semaphore>>>,
    }

    impl Mock {
        /// Issues a new access/refresh token pair the MCP side accepts.
        pub fn issue(&self) -> (String, String) {
            let mut tokens = self.tokens.lock().unwrap();
            tokens.issued += 1;
            let n = tokens.issued;
            let pair = (format!("access-{n}"), format!("refresh-{n}"));
            tokens.accepted.insert(pair.0.clone());
            tokens.live_refresh.insert(pair.1.clone());
            pair
        }

        /// The MCP side stops accepting `token`, as for a token the
        /// user revoked or that expired at the provider.
        pub fn revoke(&self, token: &str) {
            self.tokens.lock().unwrap().accepted.remove(token);
        }

        fn accepts(&self, token: &str) -> bool {
            self.tokens.lock().unwrap().accepted.contains(token)
        }

        pub fn refresh_count(&self) -> u32 {
            self.refreshes.load(Ordering::SeqCst)
        }

        pub fn initialize_count(&self) -> u32 {
            self.initializes.load(Ordering::SeqCst)
        }
    }

    fn unauthorized() -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer error=\"invalid_token\"")],
        )
            .into_response()
    }

    fn bearer(headers: &HeaderMap) -> Option<String> {
        headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")
            .map(str::to_string)
    }

    async fn mcp_handler(
        State(mock): State<Arc<Mock>>,
        headers: HeaderMap,
        axum::Json(request): axum::Json<serde_json::Value>,
    ) -> Response {
        let Some(token) = bearer(&headers).filter(|t| mock.accepts(t)) else {
            return unauthorized();
        };
        let Some(id) = request.get("id").cloned() else {
            // A notification (`notifications/initialized`).
            return StatusCode::ACCEPTED.into_response();
        };
        let result = match request["method"].as_str() {
            Some("initialize") => {
                mock.initializes.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({
                    "protocolVersion": request["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock", "version": "0"},
                })
            }
            Some("tools/list") => serde_json::json!({"tools": [
                {"name": "echo", "description": "Echoes", "inputSchema": {"type": "object"}}
            ]}),
            Some("tools/call") if request["params"]["name"] == "fail" => {
                return axum::Json(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": "the tool refused"},
                }))
                .into_response();
            }
            Some("tools/call") => serde_json::json!({
                "content": [{"type": "text", "text": format!("called with {token}")}],
                "isError": false,
            }),
            _ => {
                return axum::Json(serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"},
                }))
                .into_response();
            }
        };
        axum::Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
    }

    fn token_error(status: StatusCode, error: &str) -> Response {
        (status, axum::Json(serde_json::json!({"error": error}))).into_response()
    }

    /// RFC 6749 §2.3.1's HTTP Basic client authentication, as oauth2
    /// sends it: each part form-urlencoded, then base64.
    fn basic_auth(client_id: &str, secret: &str) -> String {
        let encode = |s: &str| -> String { url::form_urlencoded::byte_serialize(s.as_bytes()).collect() };
        let raw = format!("{}:{}", encode(client_id), encode(secret));
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in raw.as_bytes().chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        format!("Basic {out}")
    }

    /// A provider finishes a token request it has received even if the
    /// client hangs up meanwhile (it rotates the refresh token either
    /// way), so the work runs in a task of its own.
    async fn token_handler(
        State(mock): State<Arc<Mock>>,
        headers: HeaderMap,
        Form(form): Form<HashMap<String, String>>,
    ) -> Response {
        tokio::spawn(token_reply(mock, headers, form))
            .await
            .expect("token reply task")
    }

    async fn token_reply(
        mock: Arc<Mock>,
        headers: HeaderMap,
        form: HashMap<String, String>,
    ) -> Response {
        if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
            if let Some(client_id) = form.get("client_id") {
                mock.refresh_client_ids.lock().unwrap().push(client_id.clone());
            }
            let gate = mock.refresh_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                // Each permit lets exactly one refresh through.
                gate.acquire().await.expect("refresh gate").forget();
            }
            let required = mock.required_secret.lock().unwrap().clone();
            if let Some((client_id, secret)) = required {
                let header_ok = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    == Some(basic_auth(&client_id, &secret).as_str());
                let body_ok = form.get("client_secret") == Some(&secret);
                if !header_ok && !body_ok {
                    return token_error(StatusCode::UNAUTHORIZED, "invalid_client");
                }
            }
            if mock.reject_refreshes_like_github.load(Ordering::SeqCst) {
                return token_error(StatusCode::OK, "bad_refresh_token");
            }
            if mock.reject_refreshes.load(Ordering::SeqCst) {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant");
            }
            let presented = form.get("refresh_token").cloned().unwrap_or_default();
            {
                let mut tokens = mock.tokens.lock().unwrap();
                if !tokens.live_refresh.remove(&presented) {
                    return token_error(StatusCode::BAD_REQUEST, "invalid_grant");
                }
                tokens.accepted.clear();
            }
            mock.refreshes.fetch_add(1, Ordering::SeqCst);
        }
        let (access, refresh) = mock.issue();
        axum::Json(serde_json::json!({
            "access_token": access,
            "token_type": "bearer",
            "expires_in": 3600,
            "refresh_token": refresh,
        }))
        .into_response()
    }

    async fn register_handler(State(mock): State<Arc<Mock>>) -> axum::Json<serde_json::Value> {
        let n = mock.registered.fetch_add(1, Ordering::SeqCst) + 1;
        axum::Json(serde_json::json!({"client_id": format!("client-{n}"), "redirect_uris": []}))
    }

    pub async fn spawn() -> Arc<Mock> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        let mock = Arc::new(Mock {
            mcp_url: format!("http://{addr}/mcp"),
            tokens: StdMutex::default(),
            refreshes: AtomicU32::new(0),
            initializes: AtomicU32::new(0),
            registered: AtomicU32::new(0),
            reject_refreshes: AtomicBool::new(false),
            reject_refreshes_like_github: AtomicBool::new(false),
            required_secret: StdMutex::new(None),
            refresh_client_ids: StdMutex::new(Vec::new()),
            refresh_gate: StdMutex::new(None),
        });
        let app = axum::Router::new()
            .route("/mcp", post(mcp_handler))
            .route("/token", post(token_handler))
            .route("/register", post(register_handler))
            .with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
        mock
    }

    /// An `oauth` server row pointing at `mock`, with an id of its own
    /// (`next_server_id`).
    pub async fn create_server(
        pool: &sqlx::PgPool,
        mock: &Mock,
        client: Option<(&str, &str)>,
    ) -> McpServerConfig {
        let id = next_server_id(pool).await;
        crate::db::create_mcp_server_config(
            pool,
            &format!("oauth-http-{id}"),
            &mock.mcp_url,
            &HashMap::new(),
            "oauth",
            client.map(|c| c.0),
            client.map(|c| c.1),
        )
        .await
        .expect("create mcp server config")
    }

    /// Saves a grant `mock` just issued, as a completed Connect would,
    /// and returns its access token.
    pub async fn save_grant(pool: &sqlx::PgPool, server_id: i64, mock: &Mock, client_id: &str) -> String {
        let (access, refresh) = mock.issue();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let credentials = serde_json::json!({
            "client_id": client_id,
            "token_response": {
                "access_token": access,
                "token_type": "bearer",
                "expires_in": 3600,
                "refresh_token": refresh,
            },
            "granted_scopes": [],
            "token_received_at": now,
        });
        crate::db::set_mcp_server_oauth_credentials(pool, server_id, Some(credentials))
            .await
            .expect("save grant");
        access
    }

    /// Moves the stored token's receipt back two hours, past its
    /// hour-long `expires_in`: an expired token without a sleep.
    pub async fn age_token(pool: &sqlx::PgPool, server_id: i64) {
        sqlx::query(
            "UPDATE mcp_servers SET oauth_credentials = jsonb_set(oauth_credentials, '{token_received_at}', \
                 to_jsonb((oauth_credentials->>'token_received_at')::bigint - 7200)) WHERE id = $1",
        )
        .bind(server_id)
        .execute(pool)
        .await
        .expect("age token");
    }

    pub async fn config(pool: &sqlx::PgPool, server_id: i64) -> McpServerConfig {
        crate::db::get_mcp_server_config(pool, server_id)
            .await
            .expect("get config")
            .expect("config exists")
    }
}

mod gated_http;
mod client;
mod names;
mod registry;
mod spans;
mod classes;
mod connection;
mod hanging;
mod oauth;
