//! The connection to each pod's sandbox agent: dialing, the registry of
//! live connections, reconnects, and the agent's messages.

use super::*;

/// How long a request to the agent waits for its reply.
pub(super) const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte stream to a pod's agent.
pub(super) trait AgentIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AgentIo for T {}

pub(super) type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// How smelt reaches a pod's agent, and what it asks the cluster about the
/// pod along the way. `ClusterDialer` is the real one; tests put a fake
/// agent on a loopback port behind `dialer_for` instead.
pub(super) trait AgentDialer: Send + Sync {
    fn dial(&self, pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>>;
    /// Whether the pod is `Running`: a first connection waits for that.
    fn is_running(&self, pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>>;
    /// `Some(reason)` once the cluster says the pod is dead, `None` while it
    /// may still be alive. See `pod_death_reason`.
    fn death_reason(&self, pod_id: i64) -> BoxFuture<'_, Option<Option<String>>>;
}

/// A port-forward to the agent's port in the pod.
pub(super) struct ClusterDialer(kube::Client);

impl AgentDialer for ClusterDialer {
    fn dial(&self, pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>> {
        Box::pin(async move {
            let mut forward = pods_api(&self.0).portforward(&pod_name(pod_id), &[AGENT_PORT]).await?;
            let stream = require_stream(forward.take_stream(AGENT_PORT), "the agent port's port-forward")?;
            Ok(Box::new(stream) as Box<dyn AgentIo>)
        })
    }

    fn is_running(&self, pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>> {
        Box::pin(async move {
            let pod = pods_api(&self.0).get_opt(&pod_name(pod_id)).await?;
            Ok(pod.and_then(|p| p.status).and_then(|s| s.phase).as_deref() == Some("Running"))
        })
    }

    fn death_reason(&self, pod_id: i64) -> BoxFuture<'_, Option<Option<String>>> {
        Box::pin(async move { pod_death_reason(&pods_api(&self.0), &pod_name(pod_id)).await })
    }
}

/// The dialer for `pod_id`: the cluster, unless a test put a fake agent in
/// for that pod.
#[cfg_attr(not(test), allow(unused_variables))] // only a test puts a fake in
pub(super) fn dialer_for(pod_id: i64) -> Result<Arc<dyn AgentDialer>, SandboxError> {
    #[cfg(test)]
    if let Some(dialer) = test_dialers().lock().unwrap_or_else(|e| e.into_inner()).get(&pod_id) {
        return Ok(dialer.clone());
    }
    Ok(Arc::new(ClusterDialer(get()?.client.clone())))
}

/// Fake agents, by the pod id each test owns. Keyed by pod so tests running
/// in parallel never see each other's.
#[cfg(test)]
pub(super) fn test_dialers() -> &'static StdMutex<HashMap<i64, Arc<dyn AgentDialer>>> {
    static DIALERS: LazyLock<StdMutex<HashMap<i64, Arc<dyn AgentDialer>>>> = LazyLock::new(Default::default);
    &DIALERS
}

/// One pod's agent connection. The WebSocket itself belongs to two tasks
/// `connect` starts: one writes `outgoing`, the other reads the agent's
/// messages (`handle_agent_message`).
pub(super) struct TerminalConnection {
    pub(super) outgoing: mpsc::UnboundedSender<String>,
    /// Requests waiting for their reply, by request id. `None` once the
    /// connection has ended: every waiter was told, and nothing new waits.
    pub(super) pending: StdMutex<Option<HashMap<u64, tokio::sync::oneshot::Sender<Reply>>>>,
    /// The reader and writer tasks, stopped by `close`.
    pub(super) tasks: StdMutex<Vec<tokio::task::AbortHandle>>,
    /// Request ids are per connection and never reused, so a late reply
    /// can't answer a newer request.
    pub(super) next_request_id: std::sync::atomic::AtomicU64,
    /// Resolved once, when the connection is first established (see
    /// `connect`) — lets `handle_agent_message` publish a
    /// `SandboxCommandUpdate` for every output line and completion without
    /// a per-line DB round trip. See
    /// SME-10.
    pub(super) conversation_id: i64,
    /// What the agent said it speaks, in its hello.
    pub(super) agent_version: ProtocolVersion,
}

impl TerminalConnection {
    /// Sends a message that gets no reply (`command`, `signal`).
    pub(super) fn send(&self, message: &ClientMessage) -> Result<(), AgentRequestError> {
        let text = serde_json::to_string(message).map_err(|e| AgentRequestError::Unencodable(e.to_string()))?;
        self.outgoing.send(text).map_err(|_| AgentRequestError::Disconnected)
    }

    /// Sends the request `build` makes with a fresh request id, and waits
    /// for its reply. The agent's own refusal (`Reply::Error`) comes back as
    /// `Rejected`.
    pub(super) async fn request(&self, build: impl FnOnce(u64) -> ClientMessage) -> Result<Reply, AgentRequestError> {
        self.request_within(ACK_TIMEOUT, build).await
    }

    pub(super) async fn request_within(
        &self,
        timeout: Duration,
        build: impl FnOnce(u64) -> ClientMessage,
    ) -> Result<Reply, AgentRequestError> {
        let request_id = self.next_request_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        match self.pending.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some(pending) => pending.insert(request_id, tx),
            None => return Err(AgentRequestError::Disconnected),
        };
        if let Err(e) = self.send(&build(request_id)) {
            self.forget(request_id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Reply::Error { message })) => Err(AgentRequestError::Rejected(message)),
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(AgentRequestError::Disconnected),
            Err(_) => {
                self.forget(request_id);
                Err(AgentRequestError::Timeout)
            }
        }
    }

    pub(super) fn forget(&self, request_id: u64) {
        if let Some(pending) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            pending.remove(&request_id);
        }
    }

    /// Hands `reply` to the request waiting for it. A reply nobody waits for
    /// (its request timed out) is dropped.
    pub(super) fn resolve(&self, request_id: u64, reply: Reply) {
        let waiter = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|pending| pending.remove(&request_id));
        match waiter {
            Some(waiter) => {
                let _ = waiter.send(reply);
            }
            None => tracing::debug!(request_id, "a reply for a request nobody waits for any more"),
        }
    }

    /// Ends every waiting request with `Disconnected` at once, and refuses
    /// new ones: the socket is gone, so no reply is coming.
    pub(super) fn fail_pending(&self) {
        // Dropping the senders wakes their receivers.
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Fails every waiting request and stops both tasks, which closes the
    /// socket.
    pub(super) fn close(&self) {
        self.fail_pending();
        for task in self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            task.abort();
        }
    }

    /// Whether the agent has what minor `minor` of this major added. A
    /// feature added in a later minor checks this first, so an older pod
    /// fails that one tool and nothing else.
    #[cfg_attr(not(test), allow(dead_code))] // until a feature needs a minor above 0
    pub(super) fn require_minor(&self, minor: u32) -> Result<(), TerminalError> {
        if self.agent_version.minor >= minor {
            Ok(())
        } else {
            Err(TerminalError::AgentTooOld {
                needs: ProtocolVersion { major: PROTOCOL_VERSION.major, minor },
                found: self.agent_version,
            })
        }
    }
}

/// The per-pod registry — one WebSocket connection per pod, shared by
/// every terminal that pod hosts (one agent *process* per pod — see
/// SME-9's "Why N pods and N terminals"). Also what smelt knows about pods
/// it must not connect to: ones torn down, and ones whose agent is
/// outdated. Kept together under one lock, so a connection finishing
/// while its pod is torn down is either closed by the teardown or refused
/// by `register`, never kept.
#[derive(Default)]
pub(super) struct Registry {
    pub(super) connections: HashMap<i64, Arc<TerminalConnection>>,
    /// Pod ids never come back, so this is never cleared.
    pub(super) torn_down: std::collections::HashSet<i64>,
    /// Pods whose agent speaks another major (or none): `found` from its
    /// hello. The image can't change under a pod, so it doesn't expire.
    pub(super) outdated: HashMap<i64, Option<ProtocolVersion>>,
}

pub(super) static REGISTRY: LazyLock<StdMutex<Registry>> = LazyLock::new(Default::default);

pub(super) fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn registry_get(pod_id: i64) -> Option<Arc<TerminalConnection>> {
    registry().connections.get(&pod_id).cloned()
}

pub(super) fn registry_contains(pod_id: i64) -> bool {
    registry().connections.contains_key(&pod_id)
}

/// Why `register` refused a connection.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refused {
    TornDown,
    /// Its socket already closed. Its reader, finding it unregistered,
    /// took that for a deliberate teardown and won't reconnect, so a
    /// registered dead connection would never be replaced.
    Ended,
}

/// Makes `conn` the pod's connection, unless the pod was torn down or the
/// connection has already ended. Checked under the registry's lock, and the
/// reader fails its waiters before it looks in the registry, so a
/// connection either is refused here or is found there by its reader.
pub(super) fn register(pod_id: i64, conn: Arc<TerminalConnection>) -> Result<(), Refused> {
    let mut registry = registry();
    if registry.torn_down.contains(&pod_id) {
        return Err(Refused::TornDown);
    }
    if conn.pending.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
        return Err(Refused::Ended);
    }
    registry.connections.insert(pod_id, conn);
    Ok(())
}

/// The pod is being torn down: close its connection, and keep any connect
/// still under way from registering one. `terminate_pod_with` takes the mark
/// back if the teardown fails.
pub(super) fn deregister(pod_id: i64) {
    let conn = {
        let mut registry = registry();
        registry.torn_down.insert(pod_id);
        registry.outdated.remove(&pod_id);
        registry.connections.remove(&pod_id)
    };
    if let Some(conn) = conn {
        conn.close();
    }
}

/// A smelt restart, as far as one pod is concerned: its connection is
/// closed and forgotten, and the pod is left alone.
#[cfg(test)]
pub(super) fn forget_connection(pod_id: i64) {
    let conn = registry().connections.remove(&pod_id);
    if let Some(conn) = conn {
        conn.close();
    }
}

/// Like `deregister`, but only actually removes the entry — and reports
/// having done so — if it's still exactly this same connection (`Arc::ptr_eq`,
/// not just an equal `pod_id`). Returns `false` when someone else (a
/// deliberate `terminate_pod`/`teardown_conversation`, or a newer
/// reconnect) already replaced or removed it first. See SME-12's "How":
/// this is what lets `connect()`'s reader task tell "this pod was
/// deliberately torn down" apart from "this connection just crashed"
/// without any new state.
pub(super) fn deregister_if_current(pod_id: i64, conn: &Arc<TerminalConnection>) -> bool {
    let mut registry = registry();
    match registry.connections.get(&pod_id) {
        Some(current) if Arc::ptr_eq(current, conn) => {
            registry.connections.remove(&pod_id);
            true
        }
        _ => false,
    }
}

/// `Some(found)` if the pod's agent is known to be outdated.
pub(super) fn outdated(pod_id: i64) -> Option<Option<ProtocolVersion>> {
    registry().outdated.get(&pod_id).copied()
}

/// How an agent on `agent` compares with a smelt on `smelt` of the same
/// major.
pub(super) fn classify_agent(agent: ProtocolVersion, smelt: ProtocolVersion) -> crate::api::pods::AgentStatus {
    use crate::api::pods::AgentStatus;
    let version = agent.to_string();
    if agent.minor < smelt.minor {
        AgentStatus::RestartRecommended { version }
    } else {
        AgentStatus::Current { version }
    }
}

/// What smelt knows of `pod_id`'s agent: the version it said hello with,
/// or that it's outdated. `None` while smelt holds no connection to it.
pub fn agent_status(pod_id: i64) -> Option<crate::api::pods::AgentStatus> {
    let registry = registry();
    if let Some(found) = registry.outdated.get(&pod_id) {
        return Some(crate::api::pods::AgentStatus::RestartRequired {
            version: found.map(|version| version.to_string()),
        });
    }
    let conn = registry.connections.get(&pod_id)?;
    Some(classify_agent(conn.agent_version, PROTOCOL_VERSION))
}

/// Serialises connecting to one pod, so two callers make one connection.
pub(super) fn pod_connect_lock(pod_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<StdMutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(pod_id)
        .or_default()
        .clone()
}

/// Returns the existing registry entry for `pod_id` if there is one;
/// otherwise tries to (re)connect to that pod's agent. This is what makes
/// a smelt restart transparently reconnect to a still-healthy agent, *and*
/// what detects a crashed agent (pod exists, `Running`, but nothing
/// answers) — see SME-9's "Agent crash recovery" and `connect_with_retry`.
pub(super) async fn reconnect_if_needed(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Arc<TerminalConnection>, TerminalError> {
    if let Some(conn) = registry_get(pod_id) {
        return Ok(conn);
    }
    connect_with_retry(pool.clone(), pod_id, ConnectMode::Reconnect).await
}

/// Bounded retry, not measured against anything real yet — see SME-12's
/// Open Questions.
pub(super) const RECONNECT_ATTEMPTS: u32 = 3;
pub(super) const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Who is connecting, which decides what a failure means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectMode {
    /// `create_terminal`, possibly the pod's first connection ever. The pod
    /// must be `Running`, and a failure is routine (the agent may not have
    /// bound its port yet): no crash cleanup.
    First,
    /// Everyone else, who expects the agent to be there. After the last
    /// failed attempt the pod is cleaned up and stopped, as after a crash.
    Reconnect,
}

/// Why one attempt to connect failed.
#[derive(Debug)]
pub(super) enum ConnectError {
    /// Worth another try: the port-forward, the handshake or the network.
    Failed(SandboxError),
    /// The agent speaks another major version, or predates versioning. Not
    /// worth another try. `certain` is false when it only said nothing.
    Outdated { found: Option<ProtocolVersion>, certain: bool },
}

/// Connects to `pod_id`'s agent and registers the connection, retrying a
/// few times. In `Reconnect` mode, a failed attempt first asks the cluster
/// whether the pod is actually dead (a transient port-forward hiccup looks
/// the same as a dead pod), so `pod_death_reason` is the authoritative
/// signal, not the attempt itself. See SME-12's "Detection design."
///
/// A plain `fn` returning a boxed future, not `async fn`: `connect()`'s
/// reader task calls this, this calls `connect()`, and that cycle defeats
/// rustc's `Send` inference (development-process.md's documented hazard).
pub(super) fn connect_with_retry(
    pool: PgPool,
    pod_id: i64,
    mode: ConnectMode,
) -> BoxFuture<'static, Result<Arc<TerminalConnection>, TerminalError>> {
    Box::pin(async move {
        if let Some(found) = outdated(pod_id) {
            return Err(TerminalError::AgentOutdated { found });
        }
        let lock = pod_connect_lock(pod_id);
        let _connecting = lock.lock().await;
        // Another caller may have connected, or found the agent outdated,
        // while this one waited for the lock.
        if let Some(conn) = registry_get(pod_id) {
            return Ok(conn);
        }
        if let Some(found) = outdated(pod_id) {
            return Err(TerminalError::AgentOutdated { found });
        }

        let dialer = dialer_for(pod_id)?;
        if mode == ConnectMode::First && !dialer.is_running(pod_id).await? {
            return Err(TerminalError::NoPod);
        }

        let mut last_err = None;
        for attempt in 0..RECONNECT_ATTEMPTS {
            let failure = match connect(pool.clone(), pod_id, dialer.clone()).await {
                Ok(conn) => match register(pod_id, conn.clone()) {
                    Ok(()) => return Ok(conn),
                    Err(Refused::TornDown) => {
                        // Torn down while this connected: a teardown doesn't
                        // wait for the lock.
                        conn.close();
                        return Err(TerminalError::NoPod);
                    }
                    Err(Refused::Ended) => ConnectError::Failed(SandboxError::WebSocket(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    )),
                },
                Err(e) => e,
            };
            match failure {
                ConnectError::Outdated { found, certain } => {
                    tracing::warn!(pod_id, ?found, certain, "the sandbox agent is outdated");
                    // Silence might be a slow link rather than an old agent,
                    // so only a message that says so is remembered.
                    if certain {
                        registry().outdated.insert(pod_id, found);
                    }
                    return Err(TerminalError::AgentOutdated { found });
                }
                ConnectError::Failed(e) => {
                    tracing::info!(pod_id, attempt, error = %e, "couldn't connect to the sandbox agent");
                    if mode == ConnectMode::Reconnect {
                        if let Some(reason) = dialer.death_reason(pod_id).await {
                            clean_up_and_terminate_pod(&pool, pod_id, reason).await;
                            return Err(TerminalError::AgentUnreachable);
                        }
                    }
                    last_err = Some(e);
                    if attempt + 1 < RECONNECT_ATTEMPTS {
                        tokio::time::sleep(RECONNECT_BACKOFF).await;
                    }
                }
            }
        }

        match mode {
            ConnectMode::First => Err(last_err.map(TerminalError::from).unwrap_or(TerminalError::AgentUnreachable)),
            ConnectMode::Reconnect => {
                // Every attempt failed without the cluster ever confirming
                // the pod is dead (a pod stuck reporting `Running` while
                // unreachable, say). Its terminals are unusable either way,
                // so clean up and stop it the same as a confirmed crash.
                clean_up_and_terminate_pod(&pool, pod_id, None).await;
                Err(TerminalError::AgentUnreachable)
            }
        }
    })
}

/// `handle_crash_cleanup` plus a best-effort attempt to actually terminate
/// the pod — delete the k8s object, mark `sandbox_pods.terminated_at` (see
/// `force_terminate_pod`). Used by *every* path in `connect_with_retry`
/// that concludes the pod is gone, confirmed or not: Kubernetes doesn't
/// clean up after an OOM kill (or any other early exit) on its own — a
/// `Failed` pod with `restart_policy: Never` just sits there — and leaving
/// the DB row "live" would block `create_pod` from ever making a fresh one
/// until the model happened to call `terminate_pod` itself first.
pub(super) async fn clean_up_and_terminate_pod(pool: &PgPool, pod_id: i64, reason: Option<String>) {
    handle_crash_cleanup(pool, pod_id, reason).await;
    if let Err(e) = force_terminate_pod(pool, pod_id).await {
        tracing::warn!(pod_id, error = %e, "best-effort pod termination failed after a crash");
    }
}

/// Best-effort `reconnect_if_needed`, for callers that want the connection
/// registry to reflect current reality *before* reporting status — e.g.
/// `get_sandbox_state` on page load, so a terminal whose pod survived a
/// smelt restart doesn't sit showing "disconnected" until the model
/// happens to touch it next. `list_terminals`/`list_pods` themselves stay
/// passive (just read the registry, no attempt to reconnect) since they're
/// also on the model's own hot path — reconnect attempts have real
/// latency, worth paying once for a UI snapshot, not on every tool call.
/// Errors are swallowed; this is a freshness nicety, not something that
/// should turn an otherwise-successful snapshot fetch into an error.
///
/// Doesn't wait behind a connect already under way: a page load shows the
/// terminals disconnected for now instead.
pub async fn try_reconnect(pool: &PgPool, pod_id: i64) {
    if registry_contains(pod_id) || outdated(pod_id).is_some() {
        return;
    }
    if pod_connect_lock(pod_id).try_lock().is_err() {
        return;
    }
    let _ = reconnect_if_needed(pool, pod_id).await;
}

/// `create_terminal`'s connection step: the pod's agent is its own
/// `ENTRYPOINT`, so this only ever connects, never launches anything. See
/// `ConnectMode::First`.
pub(super) async fn ensure_pod_connection(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Arc<TerminalConnection>, TerminalError> {
    if let Some(conn) = registry_get(pod_id) {
        return Ok(conn);
    }
    connect_with_retry(pool.clone(), pod_id, ConnectMode::First).await
}

/// The value `pick` finds in a request's reply, or why there isn't one.
pub(super) fn expect_reply<T>(
    result: Result<Reply, AgentRequestError>,
    action: &'static str,
    pick: impl FnOnce(Reply) -> Option<T>,
) -> Result<T, TerminalError> {
    pick(result?).ok_or(TerminalError::Agent(AgentRequestError::UnexpectedReply(action)))
}

/// Opens a WebSocket to `pod_id`'s agent over `dialer` and starts the two
/// tasks that own it: one drains `outgoing` into the socket, the other
/// hands every agent message to `handle_agent_message` (output and exits
/// into `terminal_events`/`terminal_commands`, replies to their waiting
/// request). When the socket ends unexpectedly, the reader reconnects, or
/// confirms a crash. One connection per **pod**, shared by every terminal
/// it hosts — see SME-9's "Why N pods and N terminals."
pub(super) async fn connect(
    pool: PgPool,
    pod_id: i64,
    dialer: Arc<dyn AgentDialer>,
) -> Result<Arc<TerminalConnection>, ConnectError> {
    // Resolved once per pod connection, not per message — see the
    // `conversation_id` field's own doc comment on `TerminalConnection`.
    let conversation_id = db::sandbox_pod_conversation_id(&pool, pod_id)
        .await
        .map_err(|e| ConnectError::Failed(SandboxError::Db(e)))?
        .ok_or(ConnectError::Failed(SandboxError::Db(sqlx::Error::RowNotFound)))?;

    let (ws_stream, agent_version) = tokio::time::timeout(AGENT_CONNECT_TIMEOUT, async {
        let stream = dialer.dial(pod_id).await.map_err(ConnectError::Failed)?;
        let url = format!("ws://{}.sandbox-agent.local/ws", pod_name(pod_id));
        let (mut ws_stream, _response) = tokio_tungstenite::client_async(url, stream)
            .await
            .map_err(|e| ConnectError::Failed(SandboxError::WebSocket(e)))?;
        let version = read_hello(&mut ws_stream).await?;
        Ok((ws_stream, version))
    })
    .await
    .map_err(|_| ConnectError::Failed(SandboxError::AgentConnectTimeout))??;
    let (mut write, mut read) = ws_stream.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let conn = Arc::new(TerminalConnection {
        outgoing: tx,
        pending: StdMutex::new(Some(HashMap::new())),
        tasks: StdMutex::new(Vec::new()),
        next_request_id: std::sync::atomic::AtomicU64::new(1),
        conversation_id,
        agent_version,
    });

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if write.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let conn_for_pump = conn.clone();
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            if let WsMessage::Text(text) = msg {
                handle_agent_message(&pool, &conn_for_pump, &text).await;
            }
        }
        // No reply is coming for anything still waiting.
        conn_for_pump.fail_pending();
        // Only treat this as worth reacting to if nobody already tore this
        // *specific* connection down deliberately (`terminate_pod`/
        // `teardown_conversation` already deregister before they delete —
        // see SME-12's "How" on why that ordering makes this race-safe).
        // A newer reconnect already having replaced this entry counts the
        // same way: not this task's job to react to.
        if deregister_if_current(pod_id, &conn_for_pump) {
            tracing::info!(
                pod_id,
                "pod connection ended unexpectedly — attempting to reconnect or confirm a crash"
            );
            let _ = connect_with_retry(pool, pod_id, ConnectMode::Reconnect).await;
        } else {
            tracing::info!(
                pod_id,
                "pod connection ended (already torn down or replaced)"
            );
        }
    });
    conn.tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend([writer.abort_handle(), reader.abort_handle()]);

    Ok(conn)
}

/// The whole of `connect`: port-forward, WebSocket handshake and hello.
pub(super) const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the agent has to say hello once the WebSocket is open. It sends
/// it before anything else, so only an agent that predates the hello, or a
/// very slow link, takes longer.
pub(super) const HELLO_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(5) };

/// Reads the agent's hello and returns its version, if smelt speaks its
/// major. Anything else first means an agent smelt can't talk to.
pub(super) async fn read_hello<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Result<ProtocolVersion, ConnectError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let first = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => return Ok(text),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(ConnectError::Failed(SandboxError::WebSocket(e))),
                None => {
                    return Err(ConnectError::Failed(SandboxError::WebSocket(
                        tokio_tungstenite::tungstenite::Error::ConnectionClosed,
                    )));
                }
            }
        }
    })
    .await
    .map_err(|_| ConnectError::Outdated { found: None, certain: false })??;

    match serde_json::from_str::<AgentMessage>(&first) {
        Ok(AgentMessage::Hello(version)) if version.major == PROTOCOL_VERSION.major => Ok(version),
        Ok(AgentMessage::Hello(version)) => Err(ConnectError::Outdated { found: Some(version), certain: true }),
        // An agent from before the hello: whatever it sent first (a queued
        // output line, say) isn't one.
        _ => Err(ConnectError::Outdated { found: None, certain: true }),
    }
}

pub(super) async fn handle_agent_message(pool: &PgPool, conn: &Arc<TerminalConnection>, text: &str) {
    let msg = match serde_json::from_str::<AgentMessage>(text) {
        Ok(msg) => msg,
        Err(e) => {
            // Not the text: it can be a line of anything a command printed.
            tracing::warn!(
                kind = ?e.classify(),
                line = e.line(),
                column = e.column(),
                bytes = text.len(),
                "unparseable message from the sandbox agent, ignoring"
            );
            return;
        }
    };

    match msg {
        AgentMessage::Output { id, terminal_id, stream, seq, data } => {
            let seq = seq as i64;
            if let Err(e) = db::append_terminal_event(pool, &id, stream.as_str(), seq, &data).await {
                tracing::error!(command_id = %id, error = %e, "failed to record terminal output");
            }
            if let Ok(terminal_id) = terminal_id.parse::<i64>() {
                events::publish(
                    conn.conversation_id,
                    events::ConversationEvent::SandboxCommandUpdate {
                        terminal_id,
                        command_id: id,
                        command: None,
                        status: "running".to_string(),
                        exit_code: None,
                        stream: Some(stream.as_str().to_string()),
                        latest_output: Some(data),
                        position: Some(seq),
                    },
                );
            }
        }
        AgentMessage::Exit { id, terminal_id, code } => {
            if let Err(e) = db::mark_terminal_command_finished(pool, &id, code).await {
                tracing::error!(command_id = %id, error = %e, "failed to record command completion");
            }
            if let Ok(terminal_id) = terminal_id.parse::<i64>() {
                events::publish(
                    conn.conversation_id,
                    events::ConversationEvent::SandboxCommandUpdate {
                        terminal_id,
                        command_id: id,
                        command: None,
                        status: "finished".to_string(),
                        exit_code: Some(code),
                        stream: None,
                        latest_output: None,
                        position: None,
                    },
                );
            }
            // Actively wake the model rather than leaving it to the
            // passive backlog drain (which only runs the next time
            // something *else* triggers a turn) — see SME-13. `notify`
            // runs it in a task of its own: this is the per-pod WebSocket
            // reader loop, which a model round trip mustn't hold up.
            crate::turn::notify(pool, conn.conversation_id, vec![crate::turn::Notice::Wake]);
        }
        AgentMessage::Reply { request_id, result } => conn.resolve(request_id, result),
        AgentMessage::ProtocolError { request_id: Some(request_id), message } => conn.resolve(
            request_id,
            Reply::Error {
                message: format!("the sandbox agent couldn't read smelt's request ({message}); this is a smelt bug"),
            },
        ),
        AgentMessage::ProtocolError { request_id: None, message } => {
            // Not the text: serde's error quotes the values it choked on,
            // which can be part of a request's content.
            tracing::warn!(bytes = message.len(), "the sandbox agent couldn't read a message from smelt");
        }
        AgentMessage::Hello(version) => tracing::debug!(%version, "the sandbox agent said hello again"),
        AgentMessage::Unknown => {
            tracing::debug!("an event from a newer sandbox agent that this smelt doesn't know; ignored");
        }
    }
}
