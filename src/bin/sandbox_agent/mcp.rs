//! Script MCP calls (SME-359): HTTP over a Unix socket in the pod, at
//! `$SMELT_MCP_SOCKET`, forwarded up the WebSocket smelt holds to this
//! agent as `mcp_request`, and answered by smelt's `mcp_response`. The
//! agent reads neither the request nor the answer: smelt picks the status
//! and builds the body. Nothing here holds a credential, a conversation
//! id or a pod id; smelt rules a call by the connection it came on.
//!
//! A Unix socket, not a TCP port: no port-forward, preview or `webfetch`
//! route can reach it, and neither can the pod's Docker containers.

use super::*;

use agent_protocol::McpRequest;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query};
use axum::http::{StatusCode, header};
use axum::routing::post;

/// Where the socket is in the image (`docker/sandbox/Dockerfile` makes
/// `/run/smelt`, `0700`, owned by `sandbox`).
pub(super) const MCP_SOCKET_PATH: &str = "/run/smelt/mcp.sock";
/// The variable every terminal's `bash` gets, naming the socket.
pub(super) const MCP_SOCKET_ENV: &str = "SMELT_MCP_SOCKET";
/// Calls waiting on smelt at once; one more is a 429 (smelt's own limit
/// is lower; this bounds the agent's memory).
pub(super) const MAX_WAITING: usize = 16;
/// How long a call waits on smelt before the script is told the outcome
/// is unknown: past smelt's own 120 s call budget plus its connect's.
pub(super) const CALL_WAIT: Duration = Duration::from_secs(180);
/// The largest request body a script may send.
const MAX_REQUEST_BYTES: usize = 1 << 20;
/// How far up the process tree a caller is traced to a terminal.
const MAX_ANCESTRY: usize = 64;

type Answer = (u16, String);

/// The script calls waiting on smelt, and the connection to send them on.
#[derive(Default)]
pub(super) struct McpCalls {
    /// The live connection's id and its way to send a request. Locked
    /// before `waiting` whenever both are.
    link: std::sync::Mutex<Option<(u64, mpsc::UnboundedSender<AgentMessage>)>>,
    /// Waiting calls by call id, with the connection each was sent on.
    waiting: std::sync::Mutex<HashMap<u64, (u64, oneshot::Sender<Answer>)>>,
    next_call_id: AtomicU64,
    /// The socket's path once bound, for every new terminal's `bash`.
    socket_path: std::sync::Mutex<Option<String>>,
}

fn json_error(message: &str, outcome: Option<&str>) -> String {
    let mut body = serde_json::json!({ "error": message });
    if let Some(outcome) = outcome {
        body["outcome"] = serde_json::Value::from(outcome);
    }
    body.to_string()
}

/// What a script gets for a call that may already have reached smelt.
fn outcome_unknown(why: &str) -> Answer {
    (503, json_error(&format!("{why}; the call may or may not have run"), Some("unknown")))
}

impl McpCalls {
    /// Makes `connection` the one script calls go up, and fails every
    /// call still waiting on an older one: its answer would come on a
    /// socket that is going away.
    pub(super) fn attach(&self, connection: u64) -> mpsc::UnboundedReceiver<AgentMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        *link = Some((connection, tx));
        self.fail_waiting(|sent_on| sent_on != connection, "a newer connection from smelt replaced the one this call went up");
        drop(link);
        rx
    }

    /// Ends `connection`'s part: no new call goes up it, and every call
    /// waiting on it is told the outcome is unknown.
    pub(super) fn detach(&self, connection: u64) {
        let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if link.as_ref().is_some_and(|(current, _)| *current == connection) {
            *link = None;
        }
        self.fail_waiting(|sent_on| sent_on == connection, "smelt's connection to this sandbox closed while the call waited");
        drop(link);
    }

    fn fail_waiting(&self, which: impl Fn(u64) -> bool, why: &str) {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        let failed: Vec<u64> = waiting.iter().filter(|(_, (sent_on, _))| which(*sent_on)).map(|(id, _)| *id).collect();
        for call_id in failed {
            if let Some((_, waiter)) = waiting.remove(&call_id) {
                let _ = waiter.send(outcome_unknown(why));
            }
        }
    }

    /// smelt's answer to `call_id`, from `connection`. One for a call that
    /// no longer waits (it timed out, its script left, or it was failed
    /// when its connection was replaced) is dropped.
    pub(super) fn answer(&self, connection: u64, call_id: u64, status: u16, body: String) {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        match waiting.get(&call_id) {
            Some((sent_on, _)) if *sent_on == connection => {
                if let Some((_, waiter)) = waiting.remove(&call_id) {
                    let _ = waiter.send((status, body));
                }
            }
            _ => tracing::debug!(call_id, "an MCP answer for a call that no longer waits; dropped"),
        }
    }

    pub(super) fn socket_path(&self) -> Option<String> {
        self.socket_path.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sends `request` up the live connection and waits for smelt's
    /// answer, at most `CALL_WAIT`.
    async fn call(&self, command_id: Option<String>, request: McpRequest) -> Answer {
        let call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let link = self.link.lock().unwrap_or_else(|e| e.into_inner());
            let Some((connection, outgoing)) = link.as_ref() else {
                return (
                    503,
                    json_error(
                        "smelt isn't connected to this sandbox right now (it reconnects when it restarts, or when the model next uses the sandbox); nothing was sent",
                        Some("not_sent"),
                    ),
                );
            };
            let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
            if waiting.len() >= MAX_WAITING {
                return (429, json_error(&format!("{MAX_WAITING} MCP calls are already waiting in this sandbox"), None));
            }
            waiting.insert(call_id, (*connection, tx));
            if outgoing.send(AgentMessage::McpRequest { call_id, command_id, request }).is_err() {
                waiting.remove(&call_id);
                return outcome_unknown("smelt's connection to this sandbox closed as the call was sent");
            }
        }
        // Removes the waiter however this ends, the script leaving (this
        // future dropped) included.
        let _waiter = Waiter { calls: self, call_id };
        match tokio::time::timeout(CALL_WAIT, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => outcome_unknown("the call was dropped while it waited"),
            Err(_) => outcome_unknown(&format!("smelt didn't answer within {}s", CALL_WAIT.as_secs())),
        }
    }
}

/// Removes its call from `waiting` when dropped.
struct Waiter<'a> {
    calls: &'a McpCalls,
    call_id: u64,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.calls.waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.call_id);
    }
}

/// The calling process's pid, from the socket's `SO_PEERCRED`.
#[derive(Clone, Copy, Debug)]
pub(super) struct PeerPid(Option<i32>);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::UnixListener>> for PeerPid {
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        PeerPid(stream.io().peer_cred().ok().and_then(|cred| cred.pid()))
    }
}

/// `/proc/<pid>/stat`'s ppid: the second field after the closing `)` of
/// `comm`, which can itself hold spaces and parentheses.
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// The terminal command `pid` belongs to: the current command of the
/// terminal whose `bash` is its nearest ancestor. The walk stops at pid 1
/// (the agent in a pod, and the reaper of orphans), at this process, or
/// after `MAX_ANCESTRY` steps. A process double-forked away from its
/// shell belongs to none. An ended shell is skipped (SME-386 M6): its pid
/// is free, and a process the kernel gave it to is some other tree's.
pub(super) async fn command_for_pid(state: &AppState, pid: i32) -> Option<String> {
    let shells: Vec<Arc<Shell>> = state.terminals.lock().await.values().cloned().collect();
    let own = std::process::id() as i32;
    let mut at = pid;
    for _ in 0..MAX_ANCESTRY {
        if at <= 1 || at == own {
            return None;
        }
        if let Some(shell) = shells.iter().find(|shell| shell.bash_pid as i32 == at && !shell.ended.load(Ordering::SeqCst)) {
            return shell.current.lock().await.clone();
        }
        at = parent_of(at)?;
    }
    None
}

#[derive(serde::Deserialize)]
struct ListQuery {
    server: Option<String>,
}

fn respond((status, body): Answer) -> axum::response::Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn list_tools(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<PeerPid>,
    Query(query): Query<ListQuery>,
) -> axum::response::Response {
    let command_id = match peer.0 {
        Some(pid) => command_for_pid(&state, pid).await,
        None => None,
    };
    respond(state.mcp.call(command_id, McpRequest::List { server: query.server }).await)
}

async fn call_tool(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<PeerPid>,
    body: String,
) -> axum::response::Response {
    let command_id = match peer.0 {
        Some(pid) => command_for_pid(&state, pid).await,
        None => None,
    };
    respond(state.mcp.call(command_id, McpRequest::Call { body }).await)
}

pub(super) fn mcp_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/tools", get(list_tools))
        .route("/v1/call", post(call_tool))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

/// Binds the socket at `path`, `0600`, replacing a stale one an earlier
/// agent in this container left, and records the path for new terminals.
pub(super) fn bind_mcp_socket(state: &AppState, path: &str) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    *state.mcp.socket_path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_string());
    Ok(listener)
}

/// Serves script calls on `listener` until the agent ends.
pub(super) async fn serve_mcp(listener: tokio::net::UnixListener, state: Arc<AppState>) {
    let app = mcp_router(state).into_make_service_with_connect_info::<PeerPid>();
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "the MCP socket stopped serving; script MCP calls fail until the pod restarts");
    }
}
