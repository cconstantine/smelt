//! SME-167: an MCP server over real HTTP (Streamable HTTP's JSON
//! response mode) that can hold its next `initialize` or `tools/list` on
//! a gate, so a test can pin a connect, or a re-list, in flight while it
//! edits or evicts the server.

use super::*;

use std::sync::Mutex as StdMutex;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::oneshot;

/// The longest a test waits on the mock or on the code under test before
/// failing, so a regression fails the test instead of hanging it under
/// the cluster lock (SME-166's retrospective).
pub const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The header whose value each `initialize` carried is recorded.
pub const HEADER: &str = "x-test";

type Gate = (&'static str, oneshot::Sender<()>, oneshot::Receiver<bool>);

pub struct Mock {
    pub url: String,
    /// The one tool `tools/list` names, so a test can tell servers apart.
    tool: &'static str,
    gate: StdMutex<Option<Gate>>,
    /// Set by `Held::fail`: every later request gets a 500.
    failing: AtomicBool,
    /// Every `initialize` that arrived, held, failed or answered.
    initializes: std::sync::atomic::AtomicU32,
    initialize_headers: StdMutex<Vec<Option<String>>>,
}

/// A request the mock is holding (or will hold once it arrives).
pub struct Held {
    reached: oneshot::Receiver<()>,
    answer: oneshot::Sender<bool>,
}

impl Mock {
    /// Holds the next `method` request until the test releases it.
    pub fn hold(&self, method: &'static str) -> Held {
        let (reached_tx, reached) = oneshot::channel();
        let (answer, answer_rx) = oneshot::channel();
        *self.gate.lock().unwrap() = Some((method, reached_tx, answer_rx));
        Held { reached, answer }
    }

    /// How many `initialize` requests arrived: one per connect attempt.
    pub fn initializes(&self) -> u32 {
        self.initializes.load(Ordering::SeqCst)
    }

    /// The `HEADER` value each answered `initialize` carried, in order.
    pub fn initialize_headers(&self) -> Vec<Option<String>> {
        self.initialize_headers.lock().unwrap().clone()
    }
}

impl Held {
    /// Waits, at most `WAIT`, for the held request to arrive.
    pub async fn reached(&mut self) {
        tokio::time::timeout(WAIT, &mut self.reached)
            .await
            .expect("the held request didn't arrive in time")
            .expect("the mock dropped its gate");
    }

    /// Lets the held request through.
    pub fn release(self) {
        let _ = self.answer.send(true);
    }

    /// Answers the held request, and every one after it, with a 500: the
    /// server has gone down.
    pub fn fail(self) {
        let _ = self.answer.send(false);
    }
}

async fn handler(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<serde_json::Value>,
) -> Response {
    let method = request["method"].as_str().unwrap_or_default().to_string();
    if method == "initialize" {
        mock.initializes.fetch_add(1, Ordering::SeqCst);
    }
    let gate = {
        let mut gate = mock.gate.lock().unwrap();
        if gate.as_ref().is_some_and(|(held, ..)| *held == method) { gate.take() } else { None }
    };
    if let Some((_, reached, answer)) = gate {
        let _ = reached.send(());
        if !answer.await.unwrap_or(false) {
            mock.failing.store(true, Ordering::SeqCst);
        }
    }
    if mock.failing.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        // A notification (`notifications/initialized`).
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method.as_str() {
        "initialize" => {
            let header = headers.get(HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
            mock.initialize_headers.lock().unwrap().push(header);
            serde_json::json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "gated", "version": "0"},
            })
        }
        "tools/list" => serde_json::json!({"tools": [
            {"name": mock.tool, "description": "A tool", "inputSchema": {"type": "object"}}
        ]}),
        "tools/call" => serde_json::json!({
            "content": [{"type": "text", "text": format!("called {}", mock.tool)}],
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

/// Starts a mock whose only tool is `tool`.
pub async fn spawn(tool: &'static str) -> Arc<Mock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    let mock = Arc::new(Mock {
        url: format!("http://{addr}/mcp"),
        tool,
        gate: StdMutex::new(None),
        failing: AtomicBool::new(false),
        initializes: std::sync::atomic::AtomicU32::new(0),
        initialize_headers: StdMutex::new(Vec::new()),
    });
    let app = axum::Router::new()
        .route("/mcp", axum::routing::post(handler))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
    mock
}
