//! Sandbox lifecycle: a disposable Kubernetes Pod per conversation, in the
//! `smelt-park` namespace, plus (new) a persistent terminal reached through
//! a purpose-built agent injected into that pod. Pod and terminal are
//! separate, explicitly-managed lifecycles — see
//! `docs/projects/plans/sandbox-terminal.md` for the full design; the
//! original pod-only mechanism is `docs/projects/plans/k8s-sandbox.md`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use k8s_openapi::api::core::v1::{Container, Pod, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, AttachParams, DeleteParams, PostParams};
use serde::Deserialize;
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::anthropic::ContentBlock;
use crate::{db, events};

const NAMESPACE: &str = "smelt-park";
const RUNNING_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Matches `sandbox_agent`'s own `LISTEN_ADDR` port.
const AGENT_PORT: u16 = 8088;
/// The agent binary, built by `scripts/build-sandbox-agent.sh` *before*
/// this crate — see the plan's "Build ordering." A documented two-command
/// sequence rather than a `build.rs`: a build script that shells out to
/// build a *second* binary in the *same* package risks recursively
/// re-invoking its own package's build script.
static AGENT_BINARY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/target/sandbox-agent/sandbox_agent"
));

#[derive(Debug)]
pub enum SandboxError {
    Kube(kube::Error),
    /// The pod didn't reach `Running` within `RUNNING_WAIT_TIMEOUT`.
    Timeout,
    /// A pod already existed for this session but wasn't `Running` (e.g.
    /// `Terminating`, `Failed`). What to do here is an open question in
    /// the plan — not resolved, just surfaced rather than guessed at.
    ExistingPodNotRunning(String),
    Io(std::io::Error),
    WebSocket(tokio_tungstenite::tungstenite::Error),
    /// A `sandbox_pods`/`sandbox_terminals` query failed — see the plan's
    /// "How" on why pod/terminal identity is DB-backed this round.
    Db(sqlx::Error),
    /// `create_pod` refuses: this conversation already has a live pod. See
    /// docs/projects/plans/file-tools.md's "One pod per conversation."
    PodAlreadyExists,
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Kube(e) => write!(f, "kubernetes API error: {e}"),
            SandboxError::Timeout => write!(f, "timed out waiting for pod to become Running"),
            SandboxError::ExistingPodNotRunning(phase) => {
                write!(f, "existing sandbox pod is not Running (phase: {phase})")
            }
            SandboxError::Io(e) => write!(f, "I/O error reading exec output: {e}"),
            SandboxError::WebSocket(e) => write!(f, "WebSocket error talking to sandbox agent: {e}"),
            SandboxError::Db(e) => write!(f, "database error: {e}"),
            SandboxError::PodAlreadyExists => {
                write!(f, "a pod already exists for this conversation; call terminate_pod first")
            }
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<kube::Error> for SandboxError {
    fn from(e: kube::Error) -> Self {
        SandboxError::Kube(e)
    }
}

fn pods_api(client: &kube::Client) -> Api<Pod> {
    Api::namespaced(client.clone(), NAMESPACE)
}

fn pod_name(pod_id: i64) -> String {
    format!("sandbox-{pod_id}")
}

/// A sandbox pod is disposable — there's nothing inside it worth a graceful
/// in-process shutdown, so deletes skip Kubernetes' default (per-pod,
/// commonly 30s) grace period rather than waiting on it. Verified this
/// matters in practice: a plain `sleep infinity` container doesn't trap
/// `SIGTERM`, so a default-grace-period delete leaves the pod `Terminating`
/// for the full grace period before it actually disappears.
fn immediate_delete_params() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    }
}

pub struct Sandbox {
    pod_name: String,
    client: kube::Client,
    cleanup_tx: mpsc::UnboundedSender<String>,
}

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl Sandbox {
    pub async fn exec(&self, command: &[&str]) -> Result<ExecResult, SandboxError> {
        let pods = pods_api(&self.client);
        let mut attached = pods
            .exec(&self.pod_name, command.iter().copied(), &AttachParams::default())
            .await?;

        let mut stdout_reader = attached.stdout().expect("stdout requested by AttachParams::default()");
        let mut stderr_reader = attached.stderr().expect("stderr requested by AttachParams::default()");
        let mut stdout = String::new();
        let mut stderr = String::new();
        let (stdout_res, stderr_res) = tokio::join!(
            stdout_reader.read_to_string(&mut stdout),
            stderr_reader.read_to_string(&mut stderr),
        );
        stdout_res.map_err(SandboxError::Io)?;
        stderr_res.map_err(SandboxError::Io)?;

        let status = attached.take_status();
        attached.join().await.ok();
        let status = match status {
            Some(fut) => fut.await,
            None => None,
        };

        Ok(ExecResult {
            stdout,
            stderr,
            exit_code: extract_exit_code(status),
        })
    }
}

/// On success the exec protocol's terminal `Status` carries no exit code at
/// all (implying 0); on a non-zero exit it's a `StatusCause` with
/// `reason == "ExitCode"` and the code itself, as a string, in `message`.
/// Verified against a real cluster, not assumed — see the plan.
fn extract_exit_code(status: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>) -> i32 {
    status
        .and_then(|s| s.details)
        .and_then(|d| d.causes)
        .into_iter()
        .flatten()
        .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
        .and_then(|cause| cause.message)
        .and_then(|message| message.parse().ok())
        .unwrap_or(0)
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        tracing::info!(pod = %self.pod_name, "Sandbox dropped, queuing cleanup");
        let _ = self.cleanup_tx.send(self.pod_name.clone());
    }
}

pub struct SandboxManager {
    client: kube::Client,
    cleanup_tx: mpsc::UnboundedSender<String>,
}

impl SandboxManager {
    pub fn new(client: kube::Client) -> Self {
        let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel();
        tokio::spawn(drain_cleanup_queue(client.clone(), cleanup_rx));
        Self { client, cleanup_tx }
    }

    /// `memory`/`cpu` are already-resolved values (the caller's own
    /// override, or its default) — only used on the actual-creation
    /// branch below; the reuse branch has nothing to apply them to, since
    /// resources are immutable on an already-existing pod.
    pub async fn create(&self, session_id: &str, memory: &str, cpu: &str) -> Result<Sandbox, SandboxError> {
        let pods = pods_api(&self.client);
        let name = format!("sandbox-{session_id}");

        match pods.get_opt(&name).await? {
            Some(pod) => {
                let phase = pod.status.and_then(|s| s.phase).unwrap_or_default();
                if phase != "Running" {
                    // Non-Running existing pod (Terminating, Failed, ...)
                    // is still an open question per the plan — not
                    // handled yet.
                    return Err(SandboxError::ExistingPodNotRunning(phase));
                }
                // Reuse: what makes an active conversation's sandbox
                // survive a smelt server restart, see the plan's "Restart
                // behavior" section.
            }
            None => {
                pods.create(&PostParams::default(), &build_pod_spec(&name, memory, cpu)).await?;
            }
        }

        wait_for_running(&pods, &name).await?;

        Ok(Sandbox {
            pod_name: name,
            client: self.client.clone(),
            cleanup_tx: self.cleanup_tx.clone(),
        })
    }

    pub async fn delete(&self, sandbox: Sandbox) -> Result<(), SandboxError> {
        let pods = pods_api(&self.client);
        pods.delete(&sandbox.pod_name, &immediate_delete_params()).await?;
        // Disarms Drop: safe to skip since none of Sandbox's fields have
        // meaningful Drop side effects of their own (a String, a
        // cheaply-Clone/Arc-backed kube::Client, an UnboundedSender whose
        // Drop is just a refcount decrement) — see the plan.
        std::mem::forget(sandbox);
        Ok(())
    }
}

/// `SANDBOX_MEMORY_LIMIT`, default `"8Gi"` if unset or empty — same
/// pattern `api::chat::anthropic_model()` uses for `ANTHROPIC_MODEL`. The
/// *default* a pod gets when `create_pod`'s caller doesn't specify its own
/// `memory_limit` — see the plan's "Per-pod limit overrides."
fn default_memory_limit() -> String {
    std::env::var("SANDBOX_MEMORY_LIMIT").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "8Gi".to_string())
}

/// `SANDBOX_CPU_LIMIT`, default `"1"` — see `default_memory_limit`.
fn default_cpu_limit() -> String {
    std::env::var("SANDBOX_CPU_LIMIT").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "1".to_string())
}

/// `memory`/`cpu` are plain Kubernetes `Quantity` strings (`"8Gi"`, `"1"`)
/// — no app-side parsing or validation of the format; an invalid value is
/// rejected by the Kubernetes API itself when the pod is actually
/// created, surfacing back through `SandboxError::Kube` as an ordinary
/// error. Bounded from above by the `smelt-park` namespace's own
/// `LimitRange` (`k8s/smelt-park-rbac.yaml`), not by anything here.
fn build_pod_spec(name: &str, memory: &str, cpu: &str) -> Pod {
    let mut limits = std::collections::BTreeMap::new();
    limits.insert("memory".to_string(), Quantity(memory.to_string()));
    limits.insert("cpu".to_string(), Quantity(cpu.to_string()));

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "sandbox".to_string(),
                // debian:trixie-slim, not busybox:1.36 — matches the dev
                // image's own Debian release, so the sandbox actually has
                // real bash (needed for sandbox_agent's inner shell) and
                // the agent's dynamically-linked glibc dependency is
                // satisfied by construction. See the plan's "What" and its
                // Open Questions on this deliberate coupling.
                image: Some("debian:trixie-slim".to_string()),
                // Keeps the pod alive across multiple `exec` calls over a
                // conversation's lifetime; the actual work all happens via
                // exec, never via the pod's own entrypoint.
                command: Some(vec!["sleep".to_string(), "infinity".to_string()]),
                resources: Some(ResourceRequirements { limits: Some(limits), ..Default::default() }),
                ..Default::default()
            }],
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    }
}

async fn wait_for_running(pods: &Api<Pod>, name: &str) -> Result<(), SandboxError> {
    tokio::time::timeout(RUNNING_WAIT_TIMEOUT, async {
        loop {
            let pod = pods.get(name).await?;
            if pod.status.and_then(|s| s.phase).as_deref() == Some("Running") {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .map_err(|_| SandboxError::Timeout)?
}

async fn drain_cleanup_queue(client: kube::Client, mut rx: mpsc::UnboundedReceiver<String>) {
    let pods = pods_api(&client);
    while let Some(name) = rx.recv().await {
        match tokio::time::timeout(Duration::from_secs(30), pods.delete(&name, &immediate_delete_params())).await {
            Ok(Ok(_)) => tracing::info!(pod = %name, "cleaned up dropped sandbox"),
            Ok(Err(e)) => tracing::error!(pod = %name, error = %e, "failed to clean up dropped sandbox"),
            Err(_) => tracing::error!(pod = %name, "timed out cleaning up dropped sandbox"),
        }
    }
}

// --- Process-global manager singleton (mirrors db::init()/db::get()) ---

static MANAGER: OnceLock<SandboxManager> = OnceLock::new();

pub async fn init() -> &'static SandboxManager {
    let client = kube::Client::try_default()
        .await
        .unwrap_or_else(|e| panic!("failed to build kube client (check KUBECONFIG): {e}"));
    MANAGER
        .set(SandboxManager::new(client))
        .ok()
        .expect("sandbox already initialized");
    MANAGER.get().unwrap()
}

pub fn get() -> &'static SandboxManager {
    MANAGER.get().expect("sandbox not initialized; call sandbox::init() first")
}

// --- Terminal: pod, terminal, and command are three separate, explicitly
// guarded lifecycles, and a conversation may have N pods each with N
// terminals. See the plan's "What" and "How". ---

#[derive(Debug)]
pub enum TerminalError {
    Sandbox(SandboxError),
    /// A call referencing a `pod_id` that doesn't exist (never created,
    /// already terminated, or not `Running`).
    NoPod,
    /// A call referencing a `terminal_id` that doesn't exist, or whose
    /// pod's agent is unreachable (a failed reconnect after the agent was
    /// found unreachable also surfaces this, after crash-cleanup runs).
    NoTerminal,
    /// `terminate_pod` refuses while that pod still has a live terminal.
    TerminalStillExists,
    /// `terminate_terminal` refuses while a command is still `running` in
    /// that terminal.
    CommandStillRunning,
    /// A `read_file`/`write_file`/`edit_file`/`list_directory` call the
    /// agent rejected — hash mismatch, not found, ambiguous match, over
    /// the size cap, etc. Carries the agent's own message straight
    /// through, unlike `create_terminal`/`terminate_terminal`'s acks
    /// (which only ever distinguish success from a generic failure) —
    /// the model needs to see and act on exactly what went wrong.
    FileOperation(String),
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TerminalError::Sandbox(e) => write!(f, "{e}"),
            TerminalError::NoPod => {
                write!(f, "no such pod (it doesn't exist, isn't Running, or was already terminated)")
            }
            TerminalError::NoTerminal => {
                write!(f, "no such terminal (it doesn't exist, or its pod's agent is unreachable)")
            }
            TerminalError::TerminalStillExists => {
                write!(f, "this pod still has a live terminal; call terminate_terminal on it first")
            }
            TerminalError::CommandStillRunning => {
                write!(f, "a command is still running in this terminal; send_signal or wait for it to finish first")
            }
            TerminalError::FileOperation(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for TerminalError {}

impl From<SandboxError> for TerminalError {
    fn from(e: SandboxError) -> Self {
        TerminalError::Sandbox(e)
    }
}

impl From<kube::Error> for TerminalError {
    fn from(e: kube::Error) -> Self {
        TerminalError::Sandbox(e.into())
    }
}

impl From<sqlx::Error> for TerminalError {
    fn from(e: sqlx::Error) -> Self {
        TerminalError::Sandbox(SandboxError::Db(e))
    }
}

#[derive(Debug)]
pub struct PodInfo {
    pub pod_id: i64,
    pub status: String,
}

#[derive(Debug)]
pub struct TerminalInfo {
    pub terminal_id: i64,
    pub pod_id: i64,
    pub status: String,
}

/// `read_file`'s result — `hash` is the SHA-256 of the *full* file (not
/// just `lines`, the requested slice), what a later `edit_file`/
/// `write_file` call's `expected_hash` is checked against. See
/// docs/projects/plans/file-tools.md's "Change detection, not just 'was it
/// read.'"
#[derive(Debug, Clone, PartialEq)]
pub struct FileContents {
    pub lines: Vec<String>,
    pub total_lines: usize,
    pub hash: String,
}

/// One `list_directory` entry — `size` is only meaningful for a file.
#[derive(Debug, Clone, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

/// The parsed, successful body of a file-tool agent response — what a
/// pending `pending_file_requests` oneshot resolves to on success (an
/// `Err(String)` carries the agent's own error message instead, same as
/// `pending_acks`). One enum covering all four operations since they share
/// one correlation map (keyed by `request_id`, not tied to a
/// `terminal_id`).
#[derive(Debug, Clone, PartialEq)]
enum FileResponse {
    Read(FileContents),
    Written { hash: String },
    Edited { hash: String },
    Listed(Vec<DirEntry>),
}

/// How long `create_terminal`/`terminate_terminal` wait for the agent's ack
/// before giving up — see `request_terminal_action`. Not measured, same
/// spirit as the plan's other not-yet-sized timeouts (see Open Questions).
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

struct TerminalConnection {
    /// JSON-text messages destined for the agent — a background task (see
    /// `connect`) owns the actual WebSocket sink and drains this.
    outgoing: mpsc::UnboundedSender<String>,
    /// Resolved by the incoming-message pump when a `terminal_created`/
    /// `terminal_terminated`/`terminal_error` ack arrives for the matching
    /// `terminal_id` — see `request_terminal_action`. `send_command`/
    /// `send_signal` don't use this; they're still fire-and-forget,
    /// completion arrives later as an ordinary `exit` event.
    pending_acks: StdMutex<HashMap<i64, tokio::sync::oneshot::Sender<Result<(), String>>>>,
    /// The file-tool analog of `pending_acks` — keyed by a fresh
    /// `request_id` per call rather than `terminal_id`, since a file
    /// operation isn't tied to any one terminal (it's scoped to the pod as
    /// a whole). Resolved by `resolve_pending_file_request` when a
    /// `file_read`/`file_written`/`file_edited`/`directory_listed`/
    /// `file_error` message arrives — see `request_file_action`.
    pending_file_requests: StdMutex<HashMap<String, tokio::sync::oneshot::Sender<Result<FileResponse, String>>>>,
    /// Resolved once, when the connection is first established (see
    /// `connect`) — lets `handle_agent_message` publish a
    /// `SandboxCommandUpdate` for every output line and completion without
    /// a per-line DB round trip. See
    /// `docs/projects/completed/20260815-sandbox-visibility.md`.
    conversation_id: i64,
}

/// The per-pod registry — one WebSocket connection per pod, shared by
/// every terminal that pod hosts (one agent *process* per pod — see the
/// plan's "Why N pods and N terminals"). Holds only the connection handle,
/// nothing else (no scrollback, no exit-code slot; see the plan's
/// "sandbox.rs" bullet on why that in-memory state was removed entirely
/// once nothing needed a hot path fast enough to justify caching it).
static TERMINAL_CONNECTIONS: LazyLock<StdMutex<HashMap<i64, Arc<TerminalConnection>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn registry_get(pod_id: i64) -> Option<Arc<TerminalConnection>> {
    TERMINAL_CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner()).get(&pod_id).cloned()
}

fn registry_contains(pod_id: i64) -> bool {
    TERMINAL_CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&pod_id)
}

fn register(pod_id: i64, conn: Arc<TerminalConnection>) {
    TERMINAL_CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, conn);
}

fn deregister(pod_id: i64) {
    TERMINAL_CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner()).remove(&pod_id);
}

/// Like `deregister`, but only actually removes the entry — and reports
/// having done so — if it's still exactly this same connection (`Arc::ptr_eq`,
/// not just an equal `pod_id`). Returns `false` when someone else (a
/// deliberate `terminate_pod`/`teardown_conversation`, or a newer
/// reconnect) already replaced or removed it first. See the plan's "How":
/// this is what lets `connect()`'s reader task tell "this pod was
/// deliberately torn down" apart from "this connection just crashed"
/// without any new state.
fn deregister_if_current(pod_id: i64, conn: &Arc<TerminalConnection>) -> bool {
    let mut connections = TERMINAL_CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner());
    match connections.get(&pod_id) {
        Some(current) if Arc::ptr_eq(current, conn) => {
            connections.remove(&pod_id);
            true
        }
        _ => false,
    }
}

// --- Pod ---

/// The decision behind `create_pod`'s one-pod-per-conversation guard,
/// pulled out as a pure function over an already-fetched live-pod list so
/// it's unit-testable without a database or cluster — see
/// docs/projects/plans/file-tools.md's "One pod per conversation."
fn check_pod_guard(existing: &[db::SandboxPod]) -> Result<(), SandboxError> {
    if existing.is_empty() {
        Ok(())
    } else {
        Err(SandboxError::PodAlreadyExists)
    }
}

/// The decision behind `conversation_pod_id`: with the one-pod guard above
/// in place, a conversation's live-pod list is always 0 or 1 — this is
/// just "the first one, or NoPod," pulled out as a pure function for the
/// same reason as `check_pod_guard`.
fn resolve_pod_id(existing: &[db::SandboxPod]) -> Result<i64, TerminalError> {
    existing.first().map(|p| p.id).ok_or(TerminalError::NoPod)
}

/// Decides whether a pod is confirmed dead from its already-fetched
/// status, and what reason (if any) Kubernetes gave — pulled out as a
/// pure function over an `Option<Pod>` for the same testability reason as
/// `check_pod_guard`/`resolve_pod_id`. See the plan's "Testing": the outer
/// `Option` is "confirmed dead or not" (`None` means genuinely
/// inconclusive — `Running`/`Pending` — not "no reason"); the inner one is
/// "did Kubernetes give a specific reason for it."
fn decide_pod_death_reason(pod: Option<Pod>) -> Option<Option<String>> {
    let Some(pod) = pod else {
        return Some(None); // pod object gone entirely — confirmed dead, nothing left to inspect
    };
    let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
    if phase != Some("Failed") {
        return None; // Running, Pending, or no status yet — inconclusive, not confirmed either way
    }

    let container_terminated_reason = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|statuses| statuses.first())
        .and_then(|cs| {
            cs.state
                .as_ref()
                .and_then(|s| s.terminated.as_ref())
                .or_else(|| cs.last_state.as_ref().and_then(|s| s.terminated.as_ref()))
        })
        .and_then(|t| t.reason.clone());

    let reason = container_terminated_reason.or_else(|| pod.status.and_then(|s| s.reason));
    Some(reason)
}

/// `decide_pod_death_reason`'s real-world entry point — just the
/// `pods.get_opt` fetch, handed straight to the pure decision function.
/// An API call that itself fails is treated the same as "inconclusive"
/// (`None`), never as confirmation either way — see the plan's "How."
async fn pod_death_reason(pods: &Api<Pod>, name: &str) -> Option<Option<String>> {
    match pods.get_opt(name).await {
        Ok(pod) => decide_pod_death_reason(pod),
        Err(_) => None,
    }
}

/// Resolves "the conversation's pod" — with `create_pod`'s guard in place,
/// a conversation has at most one live pod, so every pod-scoped call
/// (`terminate_pod`, `create_terminal`, the file tools) can go straight
/// from a `conversation_id` to a `pod_id` without the model ever naming one
/// itself. See the plan's "One pod per conversation."
async fn conversation_pod_id(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
    let existing = db::list_sandbox_pods(pool, conversation_id).await?;
    resolve_pod_id(&existing)
}

/// Refuses if this conversation already has a live pod (see
/// `check_pod_guard`) — the model must `terminate_pod` before it can get
/// another. Rolls the DB row back (soft-terminates it) if the underlying
/// k8s create fails, so a failed create never leaves a pod_id `list_pods`
/// would show as live but that doesn't actually exist.
/// `memory_limit`/`cpu_limit` override the deployment's
/// `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` default for just this one
/// pod when given — see the plan's "Per-pod limit overrides."
pub async fn create_pod(
    pool: &PgPool,
    conversation_id: i64,
    memory_limit: Option<String>,
    cpu_limit: Option<String>,
) -> Result<i64, SandboxError> {
    let existing = db::list_sandbox_pods(pool, conversation_id).await.map_err(SandboxError::Db)?;
    check_pod_guard(&existing)?;

    let memory = memory_limit.unwrap_or_else(default_memory_limit);
    let cpu = cpu_limit.unwrap_or_else(default_cpu_limit);

    let manager = get();
    let row = db::create_sandbox_pod(pool, conversation_id).await.map_err(SandboxError::Db)?;
    // Reuses SandboxManager::create's existing get-or-create-on-Running
    // logic rather than duplicating it — the returned Sandbox is only a
    // handle for exec/Drop-cleanup purposes, neither of which apply here
    // (this pod persists independently of any in-process value), so it's
    // disarmed immediately, the same `mem::forget` pattern
    // `SandboxManager::delete` itself already uses for the same reason.
    match manager.create(&row.id.to_string(), &memory, &cpu).await {
        Ok(sandbox) => {
            std::mem::forget(sandbox);
            events::publish(
                conversation_id,
                events::ConversationEvent::SandboxPodUpdate {
                    pod_id: row.id,
                    status: "Running".to_string(),
                    terminated: false,
                },
            );
            Ok(row.id)
        }
        Err(e) => {
            let _ = db::terminate_sandbox_pod(pool, row.id).await;
            Err(e)
        }
    }
}

/// Takes `conversation_id`, resolved to "the conversation's pod" via
/// `conversation_pod_id` — no longer idempotent on repeat the way a
/// `pod_id`-addressed version was: once the one live pod is terminated,
/// there's no longer a live pod for this conversation to resolve, so a
/// second call fails clearly with `NoPod` ("call create_pod first") rather
/// than silently succeeding again. See the plan's "How." Refuses if the
/// pod still has a live terminal.
pub async fn terminate_pod(pool: &PgPool, conversation_id: i64) -> Result<(), TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let live_terminals = db::list_sandbox_terminals_for_pod(pool, pod_id).await?;
    if !live_terminals.is_empty() {
        return Err(TerminalError::TerminalStillExists);
    }

    match force_terminate_pod(pool, pod_id).await? {
        Some(_) => Ok(()),
        None => Err(TerminalError::NoPod),
    }
}

/// The mechanical part of tearing a pod down: delete the k8s object (if
/// it's still there), mark the DB row terminated, publish the UI event.
/// Shared by `terminate_pod` (a deliberate, guarded teardown — the guard
/// above already ensures no live terminals before this runs) and
/// `reconnect_or_confirm_crash`'s exhausted-retries fallback (unguarded —
/// nothing to check, it's already given up reaching this pod). Deregisters
/// *before* touching the k8s API, not after — see the plan's "How" on why
/// that ordering is what lets a deliberate teardown always win the race
/// against the connection's own reader task noticing the drop.
async fn force_terminate_pod(pool: &PgPool, pod_id: i64) -> Result<Option<db::SandboxPod>, SandboxError> {
    deregister(pod_id);

    let manager = get();
    let name = pod_name(pod_id);
    let pods = pods_api(&manager.client);
    if pods.get_opt(&name).await?.is_some() {
        pods.delete(&name, &immediate_delete_params()).await?;
    }

    let row = db::terminate_sandbox_pod(pool, pod_id).await.map_err(SandboxError::Db)?;
    if let Some(row) = &row {
        events::publish(
            row.conversation_id,
            events::ConversationEvent::SandboxPodUpdate {
                pod_id,
                status: "terminated".to_string(),
                terminated: true,
            },
        );
    }
    Ok(row)
}

pub async fn list_pods(pool: &PgPool, conversation_id: i64) -> Result<Vec<PodInfo>, SandboxError> {
    let manager = get();
    let pods = pods_api(&manager.client);
    let rows = db::list_sandbox_pods(pool, conversation_id).await.map_err(SandboxError::Db)?;
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let name = pod_name(row.id);
        let status = pods
            .get_opt(&name)
            .await?
            .and_then(|p| p.status)
            .and_then(|s| s.phase)
            .unwrap_or_else(|| "Unknown".to_string());
        result.push(PodInfo { pod_id: row.id, status });
    }
    Ok(result)
}

// --- Terminal ---

/// Takes `conversation_id`, resolved to "the conversation's pod" via
/// `conversation_pod_id` — errors `NoPod` if none exists yet (create_pod
/// first), same requirement as before, just checked against the DB instead
/// of trusting a caller-supplied `pod_id`. Every call creates a genuinely
/// new terminal in that pod — no idempotency to preserve with N terminals
/// per pod. Establishes the pod's agent connection if it isn't already
/// live, injecting and launching the agent on first use for this pod (the
/// only place that does — see `reconnect_if_needed`, which every other
/// terminal-touching call uses instead and which never launches a fresh
/// agent itself).
pub async fn create_terminal(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = ensure_pod_connection(pool, pod_id).await?;

    let row = db::create_sandbox_terminal(pool, pod_id).await?;
    let terminal_id = row.id;

    if let Err(e) = request_terminal_action(&conn, terminal_id, "create_terminal").await {
        let _ = db::terminate_sandbox_terminal(pool, terminal_id).await;
        return Err(e);
    }
    events::publish(
        conn.conversation_id,
        events::ConversationEvent::SandboxTerminalUpdate {
            pod_id,
            terminal_id,
            status: "connected".to_string(),
            terminated: false,
        },
    );
    Ok(terminal_id)
}

/// Idempotent on repeat (see the plan's "How"). Refuses if a command is
/// still `running` in this terminal — the model must `send_signal`/wait
/// it out first. Otherwise asks the agent to `killpg` just this
/// terminal's shell — see the plan's "Terminating a terminal without
/// touching the pod, or its siblings."
pub async fn terminate_terminal(pool: &PgPool, terminal_id: i64) -> Result<(), TerminalError> {
    let Some(pod_id) = db::sandbox_terminal_pod_id(pool, terminal_id).await? else {
        return Err(TerminalError::NoTerminal);
    };

    let live = db::list_sandbox_terminals_for_pod(pool, pod_id).await?;
    if !live.iter().any(|t| t.id == terminal_id) {
        return Ok(()); // already terminated (or crash-cleanup already cleared it) — idempotent
    }

    if let Ok(Some(_)) = db::terminal_command_is_running(pool, terminal_id).await {
        return Err(TerminalError::CommandStillRunning);
    }

    let conn = reconnect_if_needed(pool, pod_id).await?;
    request_terminal_action(&conn, terminal_id, "terminate_terminal").await?;

    db::terminate_sandbox_terminal(pool, terminal_id).await?;
    events::publish(
        conn.conversation_id,
        events::ConversationEvent::SandboxTerminalUpdate {
            pod_id,
            terminal_id,
            status: "disconnected".to_string(),
            terminated: true,
        },
    );
    Ok(())
}

/// Every live terminal across every pod in the conversation, not just one
/// pod's — the model can always ask what it has without tracking pod_ids
/// itself. `status` reflects whether the *owning pod's* connection is
/// currently live, not anything about the terminal individually (there's
/// nothing per-terminal to check — one connection serves a whole pod).
pub async fn list_terminals(pool: &PgPool, conversation_id: i64) -> Result<Vec<TerminalInfo>, SandboxError> {
    let rows = db::list_sandbox_terminals_for_conversation(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    Ok(rows
        .into_iter()
        .map(|t| TerminalInfo {
            terminal_id: t.id,
            pod_id: t.pod_id,
            status: if registry_contains(t.pod_id) { "connected" } else { "disconnected" }.to_string(),
        })
        .collect())
}

/// Sends `{"action": "command", "terminal_id", "id": command_id,
/// "command"}` to the terminal's pod's agent — reconnecting first if the
/// registry has no live entry (a smelt restart, or the first send right
/// after `create_terminal`'s own connect). Never launches a fresh agent
/// itself (that's `create_terminal`'s job).
pub async fn send_command(
    pool: &PgPool,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> Result<(), TerminalError> {
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id).await?.ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let payload =
        serde_json::json!({"action": "command", "terminal_id": terminal_id.to_string(), "id": command_id, "command": command})
            .to_string();
    conn.outgoing.send(payload).map_err(|_| TerminalError::NoTerminal)
}

/// Sends `{"action": "signal", "terminal_id", "id": command_id,
/// "signal"}` — same reconnect-first, never-launches behavior as
/// `send_command`.
pub async fn send_signal(
    pool: &PgPool,
    terminal_id: i64,
    command_id: &str,
    signal: &str,
) -> Result<(), TerminalError> {
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id).await?.ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let payload =
        serde_json::json!({"action": "signal", "terminal_id": terminal_id.to_string(), "id": command_id, "signal": signal})
            .to_string();
    conn.outgoing.send(payload).map_err(|_| TerminalError::NoTerminal)
}

/// Deletes every pod that exists for this conversation, unconditionally
/// (unlike `terminate_pod`, this is a hard teardown on conversation
/// deletion, not a guarded API the model calls) — see the plan's
/// `chat.rs`/`main.rs` bullet. The DB rows themselves don't need clearing
/// here: `db::delete_conversation`'s `ON DELETE CASCADE` chain removes
/// `sandbox_pods`/`sandbox_terminals`/`terminal_commands` for real right
/// after this runs.
pub async fn teardown_conversation(pool: &PgPool, conversation_id: i64) {
    let manager = get();
    let pods = pods_api(&manager.client);
    let rows = db::list_sandbox_pods(pool, conversation_id).await.unwrap_or_default();
    for row in rows {
        deregister(row.id);
        let name = pod_name(row.id);
        if let Ok(Some(_)) = pods.get_opt(&name).await {
            if let Err(e) = pods.delete(&name, &immediate_delete_params()).await {
                tracing::warn!(pod = %name, error = %e, "failed to delete pod during conversation teardown");
            }
        }
    }
}

/// Returns the existing registry entry for `pod_id` if there is one;
/// otherwise tries to (re)connect to that pod's agent. This is what makes
/// a smelt restart transparently reconnect to a still-healthy agent, *and*
/// what detects a crashed agent (pod exists, `Running`, but nothing
/// answers) — see the plan's "Agent crash recovery": cleanup only, never
/// touches the pod, and does not attempt to launch a fresh agent itself
/// (that's `ensure_pod_connection`'s job, used only by `create_terminal`).
async fn reconnect_if_needed(pool: &PgPool, pod_id: i64) -> Result<Arc<TerminalConnection>, TerminalError> {
    if let Some(conn) = registry_get(pod_id) {
        return Ok(conn);
    }
    reconnect_or_confirm_crash(pool, pod_id).await
}

/// Bounded retry, not measured against anything real yet — see the plan's
/// Open Questions.
const RECONNECT_ATTEMPTS: u32 = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Tries to (re)connect to a pod's agent; if that fails, checks the pod's
/// *actual* status via the Kubernetes API before concluding anything — a
/// single failed `connect()` doesn't mean the pod is dead (a transient
/// portforward/API hiccup looks identical to one that does), so
/// `pod_death_reason` is the authoritative signal, not the connection
/// attempt itself. See the plan's "Detection design."
///
/// Written as a plain `fn` returning a boxed future, not `async fn` —
/// `connect()`'s reader task calls this, and this itself calls `connect()`
/// again on retry, and that `async fn`-to-`async fn` cycle defeats rustc's
/// `Send`-auto-trait inference (development-process.md's documented
/// hazard — the same shape as `api::chat::run_turn`/`anthropic::tools::execute`).
fn reconnect_or_confirm_crash(
    pool: &PgPool,
    pod_id: i64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Arc<TerminalConnection>, TerminalError>> + Send + '_>> {
    Box::pin(async move {
        let manager = get();
        let name = pod_name(pod_id);
        let pods = pods_api(&manager.client);

        for attempt in 0..RECONNECT_ATTEMPTS {
            match connect(&manager.client, pool.clone(), pod_id, &name).await {
                Ok(conn) => {
                    register(pod_id, conn.clone());
                    return Ok(conn);
                }
                Err(_) => match pod_death_reason(&pods, &name).await {
                    Some(reason) => {
                        clean_up_and_terminate_pod(pool, pod_id, reason).await;
                        return Err(TerminalError::NoTerminal);
                    }
                    None if attempt + 1 < RECONNECT_ATTEMPTS => {
                        tokio::time::sleep(RECONNECT_BACKOFF).await;
                    }
                    None => {}
                },
            }
        }

        // Exhausted every attempt without Kubernetes ever confirming the
        // pod is actually dead — a pod stuck reporting `Running` while
        // genuinely unreachable, say. The terminal is unusable either way,
        // so clean up and terminate it the same as a confirmed crash.
        clean_up_and_terminate_pod(pool, pod_id, None).await;
        Err(TerminalError::NoTerminal)
    })
}

/// `handle_crash_cleanup` plus a best-effort attempt to actually terminate
/// the pod — delete the k8s object, mark `sandbox_pods.terminated_at` (see
/// `force_terminate_pod`). Used by *every* path in `reconnect_or_confirm_crash`
/// that concludes the pod is gone, confirmed or not: Kubernetes doesn't
/// clean up after an OOM kill (or any other early exit) on its own — a
/// `Failed` pod with `restart_policy: Never` just sits there — and leaving
/// the DB row "live" would block `create_pod` from ever making a fresh one
/// until the model happened to call `terminate_pod` itself first.
async fn clean_up_and_terminate_pod(pool: &PgPool, pod_id: i64, reason: Option<String>) {
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
pub async fn try_reconnect(pool: &PgPool, pod_id: i64) {
    let _ = reconnect_if_needed(pool, pod_id).await;
}

/// `reconnect_if_needed`, and if that fails because the pod has never had
/// an agent launched in it at all, injects and launches one fresh. The
/// only caller with license to do that — everything else only ever
/// reconnects to an agent `create_terminal` already established. See the
/// plan's "Why N pods and N terminals."
async fn ensure_pod_connection(pool: &PgPool, pod_id: i64) -> Result<Arc<TerminalConnection>, TerminalError> {
    if let Some(conn) = registry_get(pod_id) {
        return Ok(conn);
    }

    let manager = get();
    let name = pod_name(pod_id);
    let pods = pods_api(&manager.client);
    let pod = pods.get_opt(&name).await.map_err(SandboxError::from)?;
    let Some(pod) = pod else {
        return Err(TerminalError::NoPod);
    };
    if pod.status.and_then(|s| s.phase).as_deref() != Some("Running") {
        return Err(TerminalError::NoPod);
    }

    // A single, quick connect attempt first — most calls land here because
    // an agent is already running from an earlier `create_terminal` in
    // this same pod (just not in our in-memory registry, e.g. after a
    // smelt restart), not because this is genuinely the pod's first ever
    // terminal. A failure here is routine (no agent yet), not a crash
    // signal, so this deliberately bypasses `reconnect_or_confirm_crash`'s
    // retry/force-terminate machinery — that's for callers who already
    // expect a connection to exist, which isn't true here by design.
    if let Ok(conn) = connect(&manager.client, pool.clone(), pod_id, &name).await {
        register(pod_id, conn.clone());
        return Ok(conn);
    }

    inject_and_launch(&manager.client, &name).await?;
    let conn = connect(&manager.client, pool.clone(), pod_id, &name).await?;
    register(pod_id, conn.clone());
    Ok(conn)
}

/// Sends a `create_terminal`/`terminate_terminal` protocol action and
/// blocks (up to `ACK_TIMEOUT`) for its ack — see the plan's "Request/ack
/// correlation, new this round." `send_command`/`send_signal` don't go
/// through this; they stay fire-and-forget.
async fn request_terminal_action(conn: &Arc<TerminalConnection>, terminal_id: i64, action: &str) -> Result<(), TerminalError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    conn.pending_acks.lock().unwrap_or_else(|e| e.into_inner()).insert(terminal_id, tx);

    let payload = serde_json::json!({"action": action, "terminal_id": terminal_id.to_string()}).to_string();
    if conn.outgoing.send(payload).is_err() {
        conn.pending_acks.lock().unwrap_or_else(|e| e.into_inner()).remove(&terminal_id);
        return Err(TerminalError::NoTerminal);
    }

    match tokio::time::timeout(ACK_TIMEOUT, rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(message))) => {
            tracing::warn!(%message, terminal_id, %action, "agent reported terminal action failure");
            Err(TerminalError::NoTerminal)
        }
        Ok(Err(_)) => Err(TerminalError::NoTerminal), // sender dropped — connection ended before the ack arrived
        Err(_) => {
            conn.pending_acks.lock().unwrap_or_else(|e| e.into_inner()).remove(&terminal_id);
            Err(TerminalError::NoTerminal)
        }
    }
}

/// Entropy for a file-tool `request_id` — only needs to be unique among
/// this *one pod connection's* outstanding requests (tool calls are
/// serialized per conversation by `run_turn`'s own lock, so there's never
/// more than one in flight at a time in practice), not globally unique.
fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!("freq-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos())
}

/// Sends a `read_file`/`write_file`/`edit_file`/`list_directory` protocol
/// action and blocks (up to `ACK_TIMEOUT`) for its response — the file-tool
/// analog of `request_terminal_action`, correlated by `request_id` instead
/// of `terminal_id` since a file operation isn't tied to any one terminal.
/// Unlike `request_terminal_action`, an agent-reported failure's message is
/// returned to the caller, not collapsed into a generic error — the model
/// needs to see exactly what went wrong (hash mismatch, ambiguous match,
/// size cap, ...).
async fn request_file_action(
    conn: &Arc<TerminalConnection>,
    payload: serde_json::Value,
    request_id: String,
) -> Result<FileResponse, TerminalError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    conn.pending_file_requests.lock().unwrap_or_else(|e| e.into_inner()).insert(request_id.clone(), tx);

    if conn.outgoing.send(payload.to_string()).is_err() {
        conn.pending_file_requests.lock().unwrap_or_else(|e| e.into_inner()).remove(&request_id);
        return Err(TerminalError::NoTerminal);
    }

    match tokio::time::timeout(ACK_TIMEOUT, rx).await {
        Ok(Ok(Ok(response))) => Ok(response),
        Ok(Ok(Err(message))) => Err(TerminalError::FileOperation(message)),
        Ok(Err(_)) => Err(TerminalError::NoTerminal), // sender dropped — connection ended before the response arrived
        Err(_) => {
            conn.pending_file_requests.lock().unwrap_or_else(|e| e.into_inner()).remove(&request_id);
            Err(TerminalError::NoTerminal)
        }
    }
}

/// Reads (a paginated slice of) `path` in this conversation's pod.
/// Reconnects first if needed, same as `send_command`/`send_signal` —
/// never launches a fresh agent itself.
pub async fn read_file(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    offset: u32,
    limit: u32,
) -> Result<FileContents, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let request_id = generate_request_id();
    let payload = serde_json::json!({
        "action": "read_file",
        "request_id": request_id,
        "path": path,
        "offset": offset,
        "limit": limit,
    });
    match request_file_action(&conn, payload, request_id).await? {
        FileResponse::Read(contents) => Ok(contents),
        _ => Err(TerminalError::FileOperation("agent returned an unexpected response type for read_file".to_string())),
    }
}

/// Creates or overwrites `path` in this conversation's pod. `expected_hash`
/// is `None` only for a brand-new file (the read-before-write check has
/// nothing to have read yet) — see
/// docs/projects/plans/file-tools.md's "Read-before-write discipline."
/// Returns the new content's hash.
pub async fn write_file(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    content: &str,
    expected_hash: Option<String>,
) -> Result<String, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let request_id = generate_request_id();
    let payload = serde_json::json!({
        "action": "write_file",
        "request_id": request_id,
        "path": path,
        "content": content,
        "expected_hash": expected_hash,
    });
    match request_file_action(&conn, payload, request_id).await? {
        FileResponse::Written { hash } => Ok(hash),
        _ => Err(TerminalError::FileOperation("agent returned an unexpected response type for write_file".to_string())),
    }
}

/// Applies a targeted `old_string` → `new_string` replacement to `path` in
/// this conversation's pod. `expected_hash` is always required (unlike
/// `write_file`) — `edit_file` always needs a prior read to have produced
/// the `old_string` it's matching against. `expected_line`, if set, targets
/// one specific occurrence instead of requiring a file-wide unique match —
/// see the plan's "What" on `edit_file`. Returns the new content's hash.
pub async fn edit_file(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    expected_hash: String,
    expected_line: Option<u32>,
) -> Result<String, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let request_id = generate_request_id();
    let payload = serde_json::json!({
        "action": "edit_file",
        "request_id": request_id,
        "path": path,
        "old_string": old_string,
        "new_string": new_string,
        "replace_all": replace_all,
        "expected_hash": expected_hash,
        "expected_line": expected_line,
    });
    match request_file_action(&conn, payload, request_id).await? {
        FileResponse::Edited { hash } => Ok(hash),
        _ => Err(TerminalError::FileOperation("agent returned an unexpected response type for edit_file".to_string())),
    }
}

/// Lists `path` (one level, non-recursive) in this conversation's pod.
pub async fn list_directory(pool: &PgPool, conversation_id: i64, path: &str) -> Result<Vec<DirEntry>, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let request_id = generate_request_id();
    let payload = serde_json::json!({"action": "list_directory", "request_id": request_id, "path": path});
    match request_file_action(&conn, payload, request_id).await? {
        FileResponse::Listed(entries) => Ok(entries),
        _ => Err(TerminalError::FileOperation("agent returned an unexpected response type for list_directory".to_string())),
    }
}

/// Marks every command still `running` under any of this pod's terminals
/// `'lost'` (no real exit code to report — see
/// `db::mark_terminal_command_lost`), and every one of the pod's live
/// terminals terminated — a dead agent was hosting all of them, not just
/// one. A safe no-op if the pod had no terminals.
async fn handle_crash_cleanup(pool: &PgPool, pod_id: i64, reason: Option<String>) {
    let conversation_id = db::sandbox_pod_conversation_id(pool, pod_id).await.ok().flatten();
    let mut found_live_terminal = false;
    if let Ok(terminals) = db::list_sandbox_terminals_for_pod(pool, pod_id).await {
        for terminal in terminals {
            found_live_terminal = true;
            if let Ok(Some(running)) = db::terminal_command_is_running(pool, terminal.id).await {
                let _ = db::mark_terminal_command_lost(pool, &running.command_id).await;
            }
            let _ = db::terminate_sandbox_terminal(pool, terminal.id).await;
            if let Some(conversation_id) = conversation_id {
                events::publish(
                    conversation_id,
                    events::ConversationEvent::SandboxTerminalUpdate {
                        pod_id,
                        terminal_id: terminal.id,
                        status: "disconnected".to_string(),
                        terminated: true,
                    },
                );
            }
        }
    }
    // One pod-level notification, gated on the same "found at least one
    // live terminal" condition that already makes a redundant second call
    // (e.g. the pre-existing reactive path still firing after this one
    // already ran) a harmless no-op — no separate dedup state needed. See
    // the plan's "Detection design": the reason string, when Kubernetes
    // gave one, is passed straight through rather than guessed at.
    if found_live_terminal {
        if let Some(conversation_id) = conversation_id {
            let text = match reason {
                Some(reason) => format!(
                    "Sandbox pod {pod_id} stopped unexpectedly ({reason}); every terminal running in it is no longer available."
                ),
                None => format!(
                    "Sandbox pod {pod_id} stopped unexpectedly; every terminal running in it is no longer available."
                ),
            };
            let _ = db::create_message(pool, conversation_id, "user", &[ContentBlock::Text { text }]).await;
        }
    }
    if let Some(conversation_id) = conversation_id {
        // Same active wake as a normal command exit (see
        // `handle_agent_message`'s "exit" branch) — a crash can leave a
        // command marked 'lost' with nobody proactively telling the model,
        // the identical gap. One wake covers whatever this pass just
        // marked lost; `wake_conversation`'s own no-op-when-nothing-
        // pending behavior makes this cheap even when nothing actually
        // changed. Detached for a different reason than the exit-event
        // call: this can run synchronously from *inside* an
        // already-in-progress `run_turn`/`execute()` call that's already
        // holding `conversation_id`'s lock (e.g. `run_terminal_command_tool`
        // → `sandbox::send_command` → `reconnect_if_needed` → here) —
        // awaiting `wake_conversation` directly would try to re-acquire
        // that same non-reentrant lock and deadlock, the same hazard
        // `cancel_task_tool`'s own comment already documents. See
        // docs/projects/plans/terminal-exit-notify.md.
        let pool = pool.clone();
        tokio::spawn(async move {
            let _ = crate::api::chat::wake_conversation(&pool, conversation_id).await;
        });
    }
    deregister(pod_id);
}

/// Tar-over-exec injection (the same trick `kubectl cp` itself is built
/// on) followed by a `setsid`-detached launch. The agent's own PID becomes
/// the process group `terminate_terminal` later signals — see the plan's
/// "Detached launch, concretely."
async fn inject_and_launch(client: &kube::Client, pod_name: &str) -> Result<(), SandboxError> {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(AGENT_BINARY.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "sandbox_agent", AGENT_BINARY)
            .map_err(SandboxError::Io)?;
        builder.finish().map_err(SandboxError::Io)?;
    }

    let pods = pods_api(client);
    let mut attached = pods
        .exec(
            pod_name,
            ["tar", "xf", "-", "-C", "/tmp"],
            &AttachParams::default().stdin(true),
        )
        .await?;
    let mut stdin = attached.stdin().expect("stdin requested via AttachParams::stdin(true)");
    stdin.write_all(&tar_bytes).await.map_err(SandboxError::Io)?;
    stdin.flush().await.map_err(SandboxError::Io)?;
    drop(stdin); // close stdin so `tar` sees EOF and exits
    attached.join().await.ok();

    let launch_script = "setsid /tmp/sandbox_agent > /tmp/agent.log 2>&1 < /dev/null & echo launched".to_string();
    let launch = pods.exec(pod_name, ["sh", "-c", &launch_script], &AttachParams::default()).await?;
    launch.join().await.ok();

    // Give the agent a moment to actually start listening before the
    // first connect attempt.
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// Portforward + a client-side WebSocket handshake over the forwarded
/// stream (kube's own "ws" feature covers exec/attach, not an arbitrary
/// application-level WS server like the agent's) — spawns one background
/// task that owns the connection for its whole lifetime: draining
/// `outgoing` into the WS sink, and parsing every incoming agent message
/// into `terminal_events`/`terminal_commands` via `db.rs`, or resolving a
/// pending `create_terminal`/`terminate_terminal` ack. Deregisters itself
/// on the way out, whatever the reason (clean close, error, agent crash)
/// — the next call that needs a connection detects that and reconnects or
/// reports `NoTerminal`. One connection per **pod**, shared by every
/// terminal it hosts — see the plan's "Why N pods and N terminals."
async fn connect(
    client: &kube::Client,
    pool: PgPool,
    pod_id: i64,
    pod_name: &str,
) -> Result<Arc<TerminalConnection>, SandboxError> {
    // Resolved once per pod connection, not per message — see the
    // `conversation_id` field's own doc comment on `TerminalConnection`.
    let conversation_id = db::sandbox_pod_conversation_id(&pool, pod_id)
        .await
        .map_err(SandboxError::Db)?
        .ok_or(SandboxError::Db(sqlx::Error::RowNotFound))?;

    let pods = pods_api(client);
    let mut pf = pods.portforward(pod_name, &[AGENT_PORT]).await?;
    let stream = pf
        .take_stream(AGENT_PORT)
        .expect("stream requested for the forwarded port");

    let url = format!("ws://{pod_name}.sandbox-agent.local/ws");
    let (ws_stream, _response) = tokio_tungstenite::client_async(url, stream)
        .await
        .map_err(SandboxError::WebSocket)?;
    let (mut write, mut read) = ws_stream.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let conn = Arc::new(TerminalConnection {
        outgoing: tx,
        pending_acks: StdMutex::new(HashMap::new()),
        pending_file_requests: StdMutex::new(HashMap::new()),
        conversation_id,
    });

    tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if write.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let conn_for_pump = conn.clone();
    tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            if let WsMessage::Text(text) = msg {
                handle_agent_message(&pool, &conn_for_pump, &text).await;
            }
        }
        // Only treat this as worth reacting to if nobody already tore this
        // *specific* connection down deliberately (`terminate_pod`/
        // `teardown_conversation` already deregister before they delete —
        // see the plan's "How" on why that ordering makes this race-safe).
        // A newer reconnect already having replaced this entry counts the
        // same way: not this task's job to react to.
        if deregister_if_current(pod_id, &conn_for_pump) {
            tracing::info!(pod_id, "pod connection ended unexpectedly — attempting to reconnect or confirm a crash");
            let _ = reconnect_or_confirm_crash(&pool, pod_id).await;
        } else {
            tracing::info!(pod_id, "pod connection ended (already torn down or replaced)");
        }
    });

    Ok(conn)
}

/// Mirrors `sandbox_agent::DirEntryInfo`'s wire shape — its own type isn't
/// reachable from here (a separate binary crate), so this is a parallel
/// definition, same as the rest of `AgentMessage`.
#[derive(Deserialize)]
struct AgentDirEntry {
    name: String,
    is_dir: bool,
    size: Option<u64>,
}

/// Flexible enough to cover every message shape `sandbox_agent`'s tagged
/// `ServerMessage` enum serializes to — a line/exit event names `id`;
/// a terminal-action ack names `terminal_id` (and, on failure, `message`);
/// a file-tool response names `request_id` instead, plus whichever of
/// `lines`/`total_lines`/`hash`/`entries` its `event` variant carries.
#[derive(Deserialize)]
struct AgentMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    code: Option<i32>,
    #[serde(default)]
    terminal_id: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    lines: Option<Vec<String>>,
    #[serde(default)]
    total_lines: Option<usize>,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    entries: Option<Vec<AgentDirEntry>>,
}

async fn handle_agent_message(pool: &PgPool, conn: &Arc<TerminalConnection>, text: &str) {
    let Ok(msg) = serde_json::from_str::<AgentMessage>(text) else {
        tracing::warn!(%text, "unparseable message from sandbox agent, ignoring");
        return;
    };

    match msg.event.as_deref() {
        Some("exit") => {
            if let (Some(id), Some(code)) = (msg.id.clone(), msg.code) {
                if let Err(e) = db::mark_terminal_command_finished(pool, &id, code).await {
                    tracing::error!(command_id = %id, error = %e, "failed to record command completion");
                }
                if let Some(terminal_id) = parse_terminal_id(&msg.terminal_id) {
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
                        },
                    );
                }
                // Actively wake the model rather than leaving it to the
                // passive backlog drain (which only runs the next time
                // something *else* triggers a turn) — see
                // docs/projects/plans/terminal-exit-notify.md. Detached:
                // this runs inside the per-pod WebSocket reader loop, and
                // awaiting a full model round trip here would block it from
                // processing any further output/exit events, this pod's or
                // a sibling terminal's, until the turn finishes.
                let pool = pool.clone();
                let conversation_id = conn.conversation_id;
                tokio::spawn(async move {
                    let _ = crate::api::chat::wake_conversation(&pool, conversation_id).await;
                });
            }
            return;
        }
        Some("terminal_created") | Some("terminal_terminated") => {
            resolve_pending_ack(conn, msg.terminal_id, Ok(()));
            return;
        }
        Some("terminal_error") => {
            resolve_pending_ack(conn, msg.terminal_id, Err(msg.message.unwrap_or_default()));
            return;
        }
        Some("file_read") => {
            let contents = FileContents {
                lines: msg.lines.unwrap_or_default(),
                total_lines: msg.total_lines.unwrap_or(0),
                hash: msg.hash.unwrap_or_default(),
            };
            resolve_pending_file_request(conn, msg.request_id, Ok(FileResponse::Read(contents)));
            return;
        }
        Some("file_written") => {
            resolve_pending_file_request(
                conn,
                msg.request_id,
                Ok(FileResponse::Written { hash: msg.hash.unwrap_or_default() }),
            );
            return;
        }
        Some("file_edited") => {
            resolve_pending_file_request(
                conn,
                msg.request_id,
                Ok(FileResponse::Edited { hash: msg.hash.unwrap_or_default() }),
            );
            return;
        }
        Some("directory_listed") => {
            let entries = msg
                .entries
                .unwrap_or_default()
                .into_iter()
                .map(|e| DirEntry { name: e.name, is_dir: e.is_dir, size: e.size })
                .collect();
            resolve_pending_file_request(conn, msg.request_id, Ok(FileResponse::Listed(entries)));
            return;
        }
        Some("file_error") => {
            resolve_pending_file_request(conn, msg.request_id, Err(msg.message.unwrap_or_default()));
            return;
        }
        _ => {}
    }

    if let (Some(id), Some(stream), Some(seq), Some(data)) =
        (msg.id.clone(), msg.stream.clone(), msg.seq, msg.data.clone())
    {
        if let Err(e) = db::append_terminal_event(pool, &id, &stream, seq, &data).await {
            tracing::error!(command_id = %id, error = %e, "failed to record terminal output");
        }
        if let Some(terminal_id) = parse_terminal_id(&msg.terminal_id) {
            events::publish(
                conn.conversation_id,
                events::ConversationEvent::SandboxCommandUpdate {
                    terminal_id,
                    command_id: id,
                    command: None,
                    status: "running".to_string(),
                    exit_code: None,
                    stream: Some(stream),
                    latest_output: Some(data),
                },
            );
        }
    }
}

fn parse_terminal_id(terminal_id: &Option<String>) -> Option<i64> {
    terminal_id.as_deref().and_then(|s| s.parse::<i64>().ok())
}

fn resolve_pending_ack(conn: &Arc<TerminalConnection>, terminal_id: Option<String>, result: Result<(), String>) {
    let Some(terminal_id) = parse_terminal_id(&terminal_id) else {
        return;
    };
    if let Some(tx) = conn.pending_acks.lock().unwrap_or_else(|e| e.into_inner()).remove(&terminal_id) {
        let _ = tx.send(result);
    }
}

fn resolve_pending_file_request(conn: &Arc<TerminalConnection>, request_id: Option<String>, result: Result<FileResponse, String>) {
    let Some(request_id) = request_id else {
        return;
    };
    if let Some(tx) = conn.pending_file_requests.lock().unwrap_or_else(|e| e.into_inner()).remove(&request_id) {
        let _ = tx.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_client() -> kube::Client {
        kube::Client::try_default().await.expect("KUBECONFIG must point at a reachable cluster for sandbox tests")
    }

    fn fake_pod(id: i64) -> db::SandboxPod {
        db::SandboxPod { id, conversation_id: 1, created_at: chrono::Utc::now().naive_utc(), terminated_at: None }
    }

    #[test]
    fn test_check_pod_guard_allows_when_no_live_pod_exists() {
        assert!(check_pod_guard(&[]).is_ok());
    }

    #[test]
    fn test_check_pod_guard_refuses_when_a_live_pod_already_exists() {
        let result = check_pod_guard(&[fake_pod(1)]);
        assert!(matches!(result, Err(SandboxError::PodAlreadyExists)), "expected PodAlreadyExists, got {result:?}");
    }

    #[test]
    fn test_resolve_pod_id_returns_the_one_live_pod() {
        assert_eq!(resolve_pod_id(&[fake_pod(42)]).expect("should resolve"), 42);
    }

    #[test]
    fn test_resolve_pod_id_errors_with_no_pod_when_none_live() {
        let result = resolve_pod_id(&[]);
        assert!(matches!(result, Err(TerminalError::NoPod)), "expected NoPod, got {result:?}");
    }

    use k8s_openapi::api::core::v1::{ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus};

    fn pod_with_phase(phase: &str) -> Pod {
        Pod { status: Some(PodStatus { phase: Some(phase.to_string()), ..Default::default() }), ..Default::default() }
    }

    fn failed_pod_with_container_state(state: Option<ContainerState>, last_state: Option<ContainerState>) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some("Failed".to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    state,
                    last_state,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn terminated(reason: Option<&str>) -> ContainerState {
        ContainerState {
            terminated: Some(ContainerStateTerminated { reason: reason.map(str::to_string), ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn test_decide_pod_death_reason_pod_gone_is_confirmed_dead_with_no_reason() {
        assert_eq!(decide_pod_death_reason(None), Some(None));
    }

    #[test]
    fn test_decide_pod_death_reason_failed_with_reason_in_state() {
        let pod = failed_pod_with_container_state(Some(terminated(Some("OOMKilled"))), None);
        assert_eq!(decide_pod_death_reason(Some(pod)), Some(Some("OOMKilled".to_string())));
    }

    #[test]
    fn test_decide_pod_death_reason_failed_with_reason_only_in_last_state() {
        // `state` present but not itself `terminated` (e.g. mid-restart) —
        // the restart_policy: Never quirk this project actually hits puts
        // the reason in `state`, not `last_state`, but a differently-
        // configured pod could still show up this way.
        let pod = failed_pod_with_container_state(
            Some(ContainerState::default()),
            Some(terminated(Some("Error"))),
        );
        assert_eq!(decide_pod_death_reason(Some(pod)), Some(Some("Error".to_string())));
    }

    #[test]
    fn test_decide_pod_death_reason_failed_falls_back_to_pod_status_reason_without_container_status() {
        let pod = Pod {
            status: Some(PodStatus {
                phase: Some("Failed".to_string()),
                reason: Some("Evicted".to_string()),
                container_statuses: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(decide_pod_death_reason(Some(pod)), Some(Some("Evicted".to_string())));
    }

    #[test]
    fn test_decide_pod_death_reason_failed_with_no_reason_available_anywhere() {
        let pod = Pod {
            status: Some(PodStatus { phase: Some("Failed".to_string()), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(decide_pod_death_reason(Some(pod)), Some(None));
    }

    #[test]
    fn test_decide_pod_death_reason_running_is_inconclusive() {
        assert_eq!(decide_pod_death_reason(Some(pod_with_phase("Running"))), None);
    }

    #[test]
    fn test_decide_pod_death_reason_pending_is_inconclusive() {
        assert_eq!(decide_pod_death_reason(Some(pod_with_phase("Pending"))), None);
    }

    #[test]
    fn test_decide_pod_death_reason_no_status_at_all_is_inconclusive() {
        assert_eq!(decide_pod_death_reason(Some(Pod::default())), None);
    }

    /// DB-only — `create_pod`'s guard must fire *before* it ever touches
    /// the process-global `MANAGER` (unset here, since this test doesn't
    /// need a real cluster), so a refusal shows up as `PodAlreadyExists`,
    /// not a panic from `get()`.
    #[sqlx::test]
    async fn test_create_pod_refuses_before_touching_the_manager_when_a_live_pod_exists(pool: PgPool) {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        db::create_sandbox_pod(&pool, conversation.id).await.expect("create sandbox pod");

        let result = create_pod(&pool, conversation.id, None, None).await;
        assert!(matches!(result, Err(SandboxError::PodAlreadyExists)), "expected PodAlreadyExists, got {result:?}");
    }

    /// DB-only — no MANAGER touch, since `conversation_pod_id` never calls
    /// `get()`.
    #[sqlx::test]
    async fn test_conversation_pod_id_resolves_the_live_pod_and_errors_with_no_pod_otherwise(pool: PgPool) {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");

        let before = conversation_pod_id(&pool, conversation.id).await;
        assert!(matches!(before, Err(TerminalError::NoPod)), "expected NoPod before any pod exists, got {before:?}");

        let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create sandbox pod");
        let resolved = conversation_pod_id(&pool, conversation.id).await.expect("should resolve");
        assert_eq!(resolved, pod.id);
    }

    fn unique_session_id(label: &str) -> String {
        format!("test-{label}-{}", uuid_like())
    }

    // Not a real UUID — just enough entropy to avoid pod-name collisions
    // between concurrent test runs, without adding a `uuid` dependency.
    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!("{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
    }

    /// A single, comprehensive, real-cluster-and-real-Postgres integration
    /// test covering the terminal *and* file-tool lifecycle end to end,
    /// including the one-pod-per-conversation guard — deliberately one
    /// large test, not many small ones: the free functions (`create_pod`,
    /// `create_terminal`, ...) reach through the process-global `MANAGER`
    /// singleton (mirroring `db::init()`/`db::get()`), and initializing it
    /// from more than one `#[tokio::test]`/`#[sqlx::test]` function would
    /// risk the same cross-runtime-reuse hazard `docs/testing.md` documents
    /// for `PgPool` (each test gets its own tokio runtime) — this is the
    /// one place in the whole suite that touches `MANAGER` at all, so
    /// there's nothing to race with. Pod isolation, previously shown via
    /// two pods in one conversation, now uses two separate conversations —
    /// a conversation can have at most one live pod, see
    /// docs/projects/plans/file-tools.md's "One pod per conversation."
    #[sqlx::test]
    async fn test_terminal_lifecycle_end_to_end(pool: PgPool) {
        // Every terminal command that finishes during this test now
        // triggers a detached `chat::wake_conversation` call (see
        // docs/projects/plans/terminal-exit-notify.md) — without this
        // redirect, `ANTHROPIC_API_KEY` present in this environment's real
        // process env (not just this test's own doing) would send every one
        // of those as a genuine request to the live Anthropic API. Pointed
        // instead at a local port nothing listens on, so every such call
        // fails fast with a local connection error rather than a real
        // (slow, costly, non-deterministic) network round trip. Held for
        // the test's whole duration, guarded the same way
        // `anthropic::stream`'s and `api::chat`'s own mock-upstream tests
        // already share this same process-global env var.
        let _anthropic_guard = crate::anthropic::test_support::lock_anthropic_base_url();
        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", "http://127.0.0.1:1");
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }

        let client = test_client().await;
        MANAGER.set(SandboxManager::new(client.clone())).ok();

        // pod_id/terminal_id are now DB-generated (see the plan's "How") —
        // each `#[sqlx::test]` run gets a *fresh* isolated Postgres database
        // whose identity sequences restart at 1, but this test still talks
        // to the one *real, shared* k3s cluster, so a from-scratch pod_id
        // sequence would collide with k8s pod names ("sandbox-1",
        // "sandbox-2", ...) left over from a previous or concurrent run of
        // *this specific test* — no other test function creates pods this
        // way (the rest all use `manager.create` directly with
        // nanosecond-entropy session ids, producing "sandbox-test-*"
        // names, untouched by this). A small pre-emptive wipe of the low
        // integer range this run will actually use is enough.
        let pods_precheck = pods_api(&client);
        for n in 1..=30i64 {
            pods_precheck.delete(&pod_name(n), &immediate_delete_params()).await.ok();
        }

        let conversation_a = db::create_conversation(&pool).await.expect("create conversation a");
        let conversation_b = db::create_conversation(&pool).await.expect("create conversation b");
        let conversation_c = db::create_conversation(&pool).await.expect("create conversation c");
        let conversation_d = db::create_conversation(&pool).await.expect("create conversation d");

        let outcome = tokio::time::timeout(Duration::from_secs(400), async {
            // --- Guards fire before there's anything to guard against yet ---
            let too_early = create_terminal(&pool, conversation_a.id).await;
            assert!(
                matches!(too_early, Err(TerminalError::NoPod)),
                "create_terminal before any pod exists should fail with NoPod, got {too_early:?}"
            );

            // --- One pod per conversation: create_pod refuses a second
            // live pod, list reflects reality ---
            let pod_a = create_pod(&pool, conversation_a.id, None, None).await.expect("create_pod (a) should succeed");
            let duplicate = create_pod(&pool, conversation_a.id, None, None).await;
            assert!(
                matches!(duplicate, Err(SandboxError::PodAlreadyExists)),
                "create_pod should refuse a second live pod for the same conversation, got {duplicate:?}"
            );
            let pods_listed = list_pods(&pool, conversation_a.id).await.expect("list_pods should succeed");
            assert_eq!(pods_listed.len(), 1, "exactly the one pod should be listed");
            assert_eq!(pods_listed[0].status, "Running");

            // --- Terminal: N per pod, no idempotency ---
            let terminal_a1 = create_terminal(&pool, conversation_a.id).await.expect("create_terminal (a1) should succeed");
            let terminal_a2 = create_terminal(&pool, conversation_a.id).await.expect("create_terminal (a2) should succeed");
            assert_ne!(terminal_a1, terminal_a2, "every create_terminal call should mint a distinct terminal");

            let terminals_listed = list_terminals(&pool, conversation_a.id).await.expect("list_terminals");
            assert_eq!(terminals_listed.len(), 2, "both terminals in the one pod should be listed");
            assert!(terminals_listed.iter().all(|t| t.status == "connected"));
            assert!(terminals_listed.iter().all(|t| t.pod_id == pod_a));

            // terminate_pod must now refuse for conversation_a — it still has terminals.
            let blocked = terminate_pod(&pool, conversation_a.id).await;
            assert!(
                matches!(blocked, Err(TerminalError::TerminalStillExists)),
                "terminate_pod should refuse while a terminal exists, got {blocked:?}"
            );

            // --- Two terminals in the same pod are genuinely independent ---
            run_and_wait(&pool, conversation_a.id, terminal_a1, "cd-a1", "cd /tmp").await;
            run_and_wait(&pool, conversation_a.id, terminal_a2, "cd-a2", "cd /var").await;
            let pwd_a1 = run_and_wait(&pool, conversation_a.id, terminal_a1, "pwd-a1", "pwd").await;
            let pwd_a2 = run_and_wait(&pool, conversation_a.id, terminal_a2, "pwd-a2", "pwd").await;
            assert_eq!(first_stdout_line(&pool, &pwd_a1).await, "/tmp");
            assert_eq!(first_stdout_line(&pool, &pwd_a2).await, "/var");

            // --- Concurrency: a long command in one terminal doesn't block
            // a sibling terminal in the same pod from running its own ---
            let long_id = "long-a1";
            db::create_terminal_command(&pool, conversation_a.id, terminal_a1, long_id, "sleep 5")
                .await
                .expect("create_terminal_command");
            send_command(&pool, terminal_a1, long_id, "sleep 5").await.expect("send_command");
            tokio::time::sleep(Duration::from_millis(300)).await; // let it actually start

            let quick_id = "quick-a2";
            db::create_terminal_command(&pool, conversation_a.id, terminal_a2, quick_id, "echo still_alive")
                .await
                .expect("create_terminal_command");
            send_command(&pool, terminal_a2, quick_id, "echo still_alive").await.expect("send_command");
            let quick_status = poll_until_finished(&pool, quick_id).await;
            assert_eq!(
                quick_status.status, "finished",
                "terminal_a2 should complete a command while terminal_a1's sleep is still running"
            );

            send_signal(&pool, terminal_a1, long_id, "KILL").await.expect("send_signal");
            poll_until_finished(&pool, long_id).await;

            // --- A terminal command's own exit event actively wakes the
            // model — no other trigger needed (see
            // docs/projects/plans/terminal-exit-notify.md). ANTHROPIC_API_KEY
            // isn't set in this test environment, so the resulting
            // wake_conversation call fails at the API step — but the
            // notification text is drained and durably persisted *before*
            // that failing call, and the failure itself publishes a visible
            // event; both are observable without ever touching a real (or
            // mock) Anthropic endpoint, and without this test doing anything
            // else that would otherwise trigger the passive backlog drain. ---
            let mut wake_events = events::subscribe(conversation_a.id);
            let wake_command_id = "wake-a1";
            db::create_terminal_command(&pool, conversation_a.id, terminal_a1, wake_command_id, "true")
                .await
                .expect("create_terminal_command");
            send_command(&pool, terminal_a1, wake_command_id, "true").await.expect("send_command");

            let saw_wake_failure = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    match wake_events.recv().await {
                        Ok(events::ConversationEvent::NotificationDeliveryFailed { .. }) => return true,
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            })
            .await
            .unwrap_or(false);
            assert!(
                saw_wake_failure,
                "the command's own exit event should have actively woken the model \
                 (surfacing as NotificationDeliveryFailed since ANTHROPIC_API_KEY isn't set \
                 in this test env), with no other trigger — not just sat unnotified waiting \
                 for something else to happen to the conversation"
            );

            let messages_after_wake = db::list_messages(&pool, conversation_a.id).await.expect("list_messages");
            assert!(
                messages_after_wake.iter().any(|m| {
                    m.blocks().ok().is_some_and(|blocks| {
                        blocks.iter().any(|b| matches!(
                            b,
                            crate::anthropic::ContentBlock::Text { text }
                                if text.contains(wake_command_id) && text.contains("finished")
                        ))
                    })
                }),
                "the notification message itself should be durably persisted even though \
                 the follow-up API call failed, got: {messages_after_wake:?}"
            );

            // --- Pod isolation: a file in conversation_a's pod is
            // invisible from conversation_b's — two separate conversations
            // now, since one conversation can't have two live pods. ---
            create_pod(&pool, conversation_b.id, None, None).await.expect("create_pod (b) should succeed");
            let terminal_b1 = create_terminal(&pool, conversation_b.id).await.expect("create_terminal (b1) should succeed");
            run_and_wait(&pool, conversation_a.id, terminal_a1, "write-marker", "echo marker > /tmp/isolation-marker").await;
            let check = run_and_wait(&pool, conversation_b.id, terminal_b1, "check-marker", "cat /tmp/isolation-marker 2>&1; echo EXIT:$?").await;
            let lines = db::read_terminal_output(&pool, &check, &["stdout"], 0, 10)
                .await
                .expect("read_terminal_output");
            let joined = lines.iter().map(|l| l.data.as_str()).collect::<Vec<_>>().join("\n");
            assert!(
                joined.contains("EXIT:1") || joined.contains("No such file"),
                "conversation_b's pod should not see a file written in conversation_a's, got: {joined}"
            );
            terminate_terminal(&pool, terminal_b1).await.expect("terminate_terminal (b1)");
            terminate_pod(&pool, conversation_b.id).await.expect("terminate_pod (b) should succeed");

            // --- terminate_terminal is guarded on a running command, per terminal ---
            let blocking_id = "blocking-a2";
            db::create_terminal_command(&pool, conversation_a.id, terminal_a2, blocking_id, "sleep 30")
                .await
                .expect("create_terminal_command");
            send_command(&pool, terminal_a2, blocking_id, "sleep 30").await.expect("send_command");
            tokio::time::sleep(Duration::from_millis(300)).await;
            let blocked_terminate = terminate_terminal(&pool, terminal_a2).await;
            assert!(
                matches!(blocked_terminate, Err(TerminalError::CommandStillRunning)),
                "terminate_terminal should refuse while a command is running, got {blocked_terminate:?}"
            );
            send_signal(&pool, terminal_a2, blocking_id, "KILL").await.expect("send_signal");
            poll_until_finished(&pool, blocking_id).await;

            // --- terminate_terminal on a2 leaves a1 (same pod) untouched ---
            terminate_terminal(&pool, terminal_a2)
                .await
                .expect("terminate_terminal (a2) should succeed once no command is running");
            // Idempotent on repeat.
            terminate_terminal(&pool, terminal_a2).await.expect("terminate_terminal (a2) should be idempotent");

            let still_pwd = run_and_wait(&pool, conversation_a.id, terminal_a1, "pwd-a1-again", "pwd").await;
            assert_eq!(
                first_stdout_line(&pool, &still_pwd).await, "/tmp",
                "terminal_a1 should be completely unaffected by terminating its sibling terminal_a2"
            );

            // --- list_commands is scoped per terminal, history survives termination ---
            let a2_history = db::list_terminal_commands(&pool, terminal_a2, 10)
                .await
                .expect("list_terminal_commands should still work for a terminated terminal");
            assert!(a2_history.iter().any(|c| c.command_id == blocking_id));
            assert!(
                !a2_history.iter().any(|c| c.command_id == "cd-a1"),
                "list_commands should not leak another terminal's history"
            );

            // --- File tools: write_file/read_file/edit_file/list_directory,
            // all pod-scoped (no terminal_id needed) against conversation_a's
            // still-live pod. ---
            let file_path = "/tmp/file-tools-test/example.txt";

            // write_file creates a new file — no expected_hash needed.
            let hash1 = write_file(&pool, conversation_a.id, file_path, "line one\nline two\n", None)
                .await
                .expect("write_file (create) should succeed");

            // read_file returns numbered lines, total_lines, and the same
            // hash write_file just returned.
            let read1 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
            assert_eq!(read1.lines, vec!["line one", "line two"]);
            assert_eq!(read1.total_lines, 2);
            assert_eq!(read1.hash, hash1);

            // edit_file with the correct expected_hash succeeds and returns a new hash.
            let hash2 = edit_file(&pool, conversation_a.id, file_path, "line one", "line ONE", false, hash1.clone(), None)
                .await
                .expect("edit_file should succeed");
            assert_ne!(hash2, hash1);
            let read2 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
            assert_eq!(read2.lines, vec!["line ONE", "line two"]);
            assert_eq!(read2.hash, hash2);

            // --- Staleness: a terminal command changes the file out from
            // under a stale hash — edit_file/write_file must refuse rather
            // than clobber it. ---
            run_and_wait(&pool, conversation_a.id, terminal_a1, "external-write", &format!("echo changed_externally > {file_path}")).await;
            let stale_edit = edit_file(&pool, conversation_a.id, file_path, "line ONE", "line X", false, hash2.clone(), None).await;
            assert!(
                matches!(stale_edit, Err(TerminalError::FileOperation(_))),
                "edit_file should refuse a stale expected_hash, got {stale_edit:?}"
            );
            let stale_write = write_file(&pool, conversation_a.id, file_path, "clobber", Some(hash2.clone())).await;
            assert!(
                matches!(stale_write, Err(TerminalError::FileOperation(_))),
                "write_file should refuse a stale expected_hash, got {stale_write:?}"
            );

            let read3 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
            assert_eq!(read3.lines, vec!["changed_externally"]);

            // write_file with the *current* hash succeeds (a legitimate overwrite).
            let hash4 = write_file(&pool, conversation_a.id, file_path, "alpha\nalpha\nbeta\n", Some(read3.hash.clone()))
                .await
                .expect("write_file (overwrite) with the current hash should succeed");

            // --- expected_line targets one specific occurrence among
            // identical repeated lines. ---
            let hash5 = edit_file(&pool, conversation_a.id, file_path, "alpha", "ALPHA", false, hash4.clone(), Some(2))
                .await
                .expect("edit_file with expected_line should succeed");
            let read5 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
            assert_eq!(read5.lines, vec!["alpha", "ALPHA", "beta"], "expected_line should have targeted only the second occurrence");
            assert_eq!(read5.hash, hash5);

            // --- replace_all replaces every occurrence ---
            let hash6 = write_file(&pool, conversation_a.id, file_path, "dup\ndup\ndup\n", Some(read5.hash.clone()))
                .await
                .expect("write_file (overwrite) should succeed");
            edit_file(&pool, conversation_a.id, file_path, "dup", "rep", true, hash6.clone(), None)
                .await
                .expect("edit_file with replace_all should succeed");
            let read7 = read_file(&pool, conversation_a.id, file_path, 1, 10).await.expect("read_file should succeed");
            assert_eq!(read7.lines, vec!["rep", "rep", "rep"]);

            // --- Ambiguous match without replace_all/expected_line is a clear error ---
            let ambiguous = edit_file(&pool, conversation_a.id, file_path, "rep", "x", false, read7.hash.clone(), None).await;
            assert!(
                matches!(ambiguous, Err(TerminalError::FileOperation(_))),
                "expected an ambiguous-match error, got {ambiguous:?}"
            );

            // --- read_file on a nonexistent path is a clear error, not a panic ---
            let missing = read_file(&pool, conversation_a.id, "/tmp/file-tools-test/does-not-exist.txt", 1, 10).await;
            assert!(matches!(missing, Err(TerminalError::FileOperation(_))), "expected a file error, got {missing:?}");

            // --- Oversized content is refused, not silently truncated ---
            let oversized_content = "x".repeat(300 * 1024); // over the 256 KiB cap
            let oversized = write_file(&pool, conversation_a.id, "/tmp/file-tools-test/big.txt", &oversized_content, None).await;
            assert!(matches!(oversized, Err(TerminalError::FileOperation(_))), "expected a size-limit error, got {oversized:?}");

            // --- list_directory: non-recursive, sorted, correct type/size,
            // and (via a nested path) proves write_file creates parent
            // directories. ---
            write_file(&pool, conversation_a.id, "/tmp/file-tools-test/subdir/nested.txt", "nested", None)
                .await
                .expect("write_file should create parent directories");
            let listing = list_directory(&pool, conversation_a.id, "/tmp/file-tools-test").await.expect("list_directory should succeed");
            let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, vec!["example.txt", "subdir"], "entries should be sorted alphabetically, size-limit failure excluded");
            let example_entry = listing.iter().find(|e| e.name == "example.txt").expect("example.txt should be listed");
            assert!(!example_entry.is_dir);
            assert!(example_entry.size.unwrap_or(0) > 0);
            let subdir_entry = listing.iter().find(|e| e.name == "subdir").expect("subdir should be listed");
            assert!(subdir_entry.is_dir);

            // --- Pod vanishes out from under us (deleted, evicted, node
            // lost) while smelt still thinks it's live — reconnect_or_confirm_crash
            // should run the same crash cleanup a failed `connect()` already
            // did, not just error out and leave the terminal wedged forever
            // — and now also fully terminate the pod itself (not just its
            // terminals), so a confirmed crash (OOM-killed, or any other
            // early exit) always leaves `sandbox_pods` correctly marked
            // terminated, the same as an exhausted-retries "gave up
            // without ever confirming" crash already did. A separate
            // conversation, since conversation_a's one pod slot is already
            // in use. ---
            let pod_c = create_pod(&pool, conversation_c.id, None, None).await.expect("create_pod (c) should succeed");
            let terminal_c1 = create_terminal(&pool, conversation_c.id).await.expect("create_terminal (c1) should succeed");

            let pods = pods_api(&get().client);
            pods.delete(&pod_name(pod_c), &immediate_delete_params()).await.expect("delete pod_c directly");
            deregister(pod_c);

            let err = terminate_terminal(&pool, terminal_c1).await;
            assert!(matches!(err, Err(TerminalError::NoTerminal)), "expected NoTerminal, got {err:?}");

            let live_c = db::list_sandbox_terminals_for_pod(&pool, pod_c).await.expect("list_sandbox_terminals_for_pod");
            assert!(
                live_c.is_empty(),
                "crash cleanup should have marked terminal_c1 terminated even though the pod was never found, not just on a failed connect"
            );

            // The confirmed crash above already fully terminated pod_c
            // itself (not just its terminal) — terminate_pod now correctly
            // finds no live pod left for conversation_c to resolve.
            let already_gone = terminate_pod(&pool, conversation_c.id).await;
            assert!(
                matches!(already_gone, Err(TerminalError::NoPod)),
                "terminate_pod (c) should find no live pod left — a confirmed crash already terminated it, got {already_gone:?}"
            );

            // --- try_reconnect: a still-healthy pod that just lost its
            // in-memory registry entry (e.g. a smelt restart, simulated
            // here with a bare `deregister` — the k8s pod itself is left
            // alone) should flip back to "connected" once try_reconnect
            // runs, not need an unrelated tool call to happen first.
            // Another separate conversation, same reason as pod_c. ---
            let pod_d = create_pod(&pool, conversation_d.id, None, None).await.expect("create_pod (d) should succeed");
            let terminal_d1 = create_terminal(&pool, conversation_d.id).await.expect("create_terminal (d1) should succeed");
            deregister(pod_d);

            let disconnected = list_terminals(&pool, conversation_d.id).await.expect("list_terminals");
            assert_eq!(
                disconnected.iter().find(|t| t.terminal_id == terminal_d1).map(|t| t.status.as_str()),
                Some("disconnected"),
                "deregistering should make list_terminals report this terminal as disconnected"
            );

            try_reconnect(&pool, pod_d).await;

            let reconnected = list_terminals(&pool, conversation_d.id).await.expect("list_terminals");
            assert_eq!(
                reconnected.iter().find(|t| t.terminal_id == terminal_d1).map(|t| t.status.as_str()),
                Some("connected"),
                "try_reconnect should have re-established the connection to the still-healthy pod"
            );

            terminate_terminal(&pool, terminal_d1).await.expect("terminate_terminal (d1)");
            terminate_pod(&pool, conversation_d.id).await.expect("terminate_pod (d)");

            // --- Full teardown: terminate remaining terminal, then the pod.
            // terminate_pod is no longer idempotent on repeat now that it's
            // resolved via conversation_pod_id (live pods only) instead of
            // a caller-supplied pod_id — once terminated, there's no live
            // pod left for this conversation to resolve, so a second call
            // is genuinely NoPod, same as a conversation that never had one. ---
            terminate_terminal(&pool, terminal_a1).await.expect("terminate_terminal (a1)");
            terminate_pod(&pool, conversation_a.id).await.expect("terminate_pod (a) should succeed once its terminal is gone");
            let repeat = terminate_pod(&pool, conversation_a.id).await;
            assert!(
                matches!(repeat, Err(TerminalError::NoPod)),
                "terminate_pod should no longer be idempotent — a second call resolves to NoPod, got {repeat:?}"
            );

            let pods_after = list_pods(&pool, conversation_a.id).await.expect("list_pods");
            assert!(pods_after.is_empty(), "no pods should be listed after terminating it, got {pods_after:?}");
            let terminals_after = list_terminals(&pool, conversation_a.id).await.expect("list_terminals");
            assert!(terminals_after.is_empty(), "no terminals should be listed after terminating all of them");

            // A conversation that never had a pod at all: the same NoPod, not a panic.
            let unknown = terminate_pod(&pool, 999_999_999).await;
            assert!(
                matches!(unknown, Err(TerminalError::NoPod)),
                "terminate_pod on a conversation that never had a pod should error, got {unknown:?}"
            );

            // --- Proactive crash detection: deleting a pod out from under a
            // live connection is noticed and reported without any further
            // tool call — see docs/projects/completed/20260816-sandbox-oom.md. ---
            let conversation_e = db::create_conversation(&pool).await.expect("create conversation e");
            let pod_e = create_pod(&pool, conversation_e.id, None, None).await.expect("create_pod (e) should succeed");
            let terminal_e = create_terminal(&pool, conversation_e.id).await.expect("create_terminal (e) should succeed");

            pods_api(&client).delete(&pod_name(pod_e), &immediate_delete_params()).await.expect("delete pod (e) directly");

            // Poll for the pod-level message itself, not `list_terminals`'
            // "disconnected" status — that status flips (via
            // `deregister_if_current`) *before* `handle_crash_cleanup`
            // finishes the rest of its work (marking the terminal
            // terminated, persisting this message), so it's an earlier,
            // racier signal than the thing this phase actually cares about.
            let detected = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if any_message_contains(&pool, conversation_e.id, "stopped unexpectedly").await {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
            .await;
            assert!(
                detected.is_ok(),
                "an unattributed crash (pod simply gone, no reason to report) should get the plain pod-level notification without any further tool call"
            );
            // `list_terminals` only returns *live* terminals
            // (`terminated_at IS NULL`) — once crash-cleanup has actually
            // terminated it, it's simply absent, not present-with-a-
            // disconnected-status.
            let terminals_e = list_terminals(&pool, conversation_e.id).await.expect("list_terminals");
            assert!(
                terminals_e.iter().all(|t| t.terminal_id != terminal_e),
                "terminal (e) should be terminated (and so absent from list_terminals) by the same crash-cleanup pass"
            );

            // --- Deliberate termination is never misreported as a crash —
            // the Arc::ptr_eq identity check in `deregister_if_current`
            // is what makes this race-safe against the reader task noticing
            // the same connection drop for a different reason. ---
            let conversation_f = db::create_conversation(&pool).await.expect("create conversation f");
            create_pod(&pool, conversation_f.id, None, None).await.expect("create_pod (f) should succeed");
            terminate_pod(&pool, conversation_f.id).await.expect("terminate_pod (f) should succeed");
            tokio::time::sleep(Duration::from_secs(2)).await; // give a wrongly-firing reader task a chance to misfire
            assert!(
                !any_message_contains(&pool, conversation_f.id, "stopped unexpectedly").await,
                "a deliberate terminate_pod must never produce a crash notification"
            );

            // --- Exhausting reconnect attempts without Kubernetes ever
            // confirming death still cleans up *and* force-terminates the
            // pod — verified by create_pod succeeding again immediately
            // after, not staying blocked behind a stale live-pod row. No
            // terminal is ever created here, so no agent is ever injected:
            // every connect() attempt genuinely and deterministically fails
            // while the pod itself stays Running throughout, exactly the
            // "stuck reporting Running but unreachable" case this path
            // exists for. ---
            let conversation_g = db::create_conversation(&pool).await.expect("create conversation g");
            let pod_g = create_pod(&pool, conversation_g.id, None, None).await.expect("create_pod (g) should succeed");
            let gave_up = reconnect_or_confirm_crash(&pool, pod_g).await;
            assert!(
                matches!(gave_up, Err(TerminalError::NoTerminal)),
                "should give up as NoTerminal once reconnect attempts are exhausted, got {:?}",
                gave_up.is_ok()
            );
            let pod_g_gone = pods_api(&client).get_opt(&pod_name(pod_g)).await.expect("get_opt");
            assert!(pod_g_gone.is_none(), "exhausting reconnect attempts should force-terminate the k8s pod");
            let recreated = create_pod(&pool, conversation_g.id, None, None).await;
            assert!(
                recreated.is_ok(),
                "create_pod should succeed again immediately, not stay blocked behind a stale live-pod row, got {recreated:?}"
            );

            // --- An idle terminal (nothing running in it) still gets
            // covered by the pod-level message, even though it has no
            // per-command message of its own. ---
            let conversation_h = db::create_conversation(&pool).await.expect("create conversation h");
            let pod_h = create_pod(&pool, conversation_h.id, None, None).await.expect("create_pod (h) should succeed");
            create_terminal(&pool, conversation_h.id).await.expect("create_terminal (h) should succeed");
            pods_api(&client).delete(&pod_name(pod_h), &immediate_delete_params()).await.expect("delete pod (h) directly");
            let detected_h = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if any_message_contains(&pool, conversation_h.id, "stopped unexpectedly").await {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
            .await;
            assert!(detected_h.is_ok(), "an idle terminal's pod crashing should still produce the pod-level notification");
        })
        .await;

        // Best-effort cleanup regardless of pass/fail, matching this file's
        // existing convention (real-cluster tests, no automatic isolation).
        let pods = pods_api(&get().client);
        for n in 1..=20i64 {
            pods.delete(&pod_name(n), &immediate_delete_params()).await.ok();
        }

        outcome.expect("terminal lifecycle integration test should complete within the timeout, not hang");
    }

    /// Creates and sends a command in one step, waits for it to finish,
    /// returns its `command_id` — most of this test's steps are this exact
    /// shape, this just cuts the repetition.
    async fn run_and_wait(pool: &PgPool, conversation_id: i64, terminal_id: i64, command_id: &str, command: &str) -> String {
        db::create_terminal_command(pool, conversation_id, terminal_id, command_id, command)
            .await
            .expect("create_terminal_command");
        send_command(pool, terminal_id, command_id, command).await.expect("send_command");
        poll_until_finished(pool, command_id).await;
        command_id.to_string()
    }

    async fn first_stdout_line(pool: &PgPool, command_id: &str) -> String {
        let lines = db::read_terminal_output(pool, command_id, &["stdout"], 0, 1)
            .await
            .expect("read_terminal_output");
        lines.first().map(|l| l.data.clone()).unwrap_or_default()
    }

    async fn poll_until_finished(pool: &PgPool, command_id: &str) -> db::TerminalCommandStatus {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let status = db::terminal_command_status(pool, command_id)
                .await
                .expect("terminal_command_status")
                .expect("command should exist");
            if status.status != "running" {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "command {command_id} did not finish in time"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn any_message_contains(pool: &PgPool, conversation_id: i64, needle: &str) -> bool {
        db::list_messages(pool, conversation_id)
            .await
            .expect("list_messages")
            .iter()
            .any(|m| m.blocks().ok().is_some_and(|blocks| {
                blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains(needle)))
            }))
    }

    #[tokio::test]
    async fn test_create_reaches_running_from_clean_slate() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("create");

        let sandbox = manager.create(&session_id, "8Gi", "1").await.expect("create should succeed");

        let pods = pods_api(&client);
        let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
        assert_eq!(pod.status.and_then(|s| s.phase).as_deref(), Some("Running"));

        pods.delete(&sandbox.pod_name, &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);
    }

    #[tokio::test]
    async fn test_create_applies_the_given_memory_and_cpu_limits() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("limits");

        let sandbox = manager.create(&session_id, "128Mi", "2").await.expect("create should succeed");

        let pods = pods_api(&client);
        let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
        let limits = pod
            .spec
            .expect("pod should have a spec")
            .containers
            .into_iter()
            .next()
            .expect("pod should have a container")
            .resources
            .expect("container should have resources")
            .limits
            .expect("resources should have limits");
        assert_eq!(limits.get("memory"), Some(&Quantity("128Mi".to_string())));
        assert_eq!(limits.get("cpu"), Some(&Quantity("2".to_string())));

        pods.delete(&sandbox.pod_name, &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);
    }

    /// A `memory_limit` over the `smelt-park` namespace's `LimitRange` max
    /// (`k8s/smelt-park-rbac.yaml`, `64Gi`) is rejected by Kubernetes
    /// itself — no app-side comparison logic to test, just that the
    /// rejection actually happens and surfaces as an ordinary error.
    /// Requires the `LimitRange` to actually be applied to the cluster
    /// (`k3s-bootstrap`, or the equivalent on `homelab`) — see the plan's
    /// "Per-pod limit overrides".
    #[tokio::test]
    async fn test_create_rejects_a_memory_limit_over_the_limitrange_max() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("over-limit");

        let result = manager.create(&session_id, "128Gi", "1").await;
        assert!(
            matches!(result, Err(SandboxError::Kube(_))),
            "a memory_limit over the LimitRange's 64Gi max should be rejected by Kubernetes, got is_ok={}",
            result.is_ok()
        );
    }

    /// The one real, permanent OOM trigger in this suite (see the plan's
    /// "Testing" on why this isn't a full `cargo build` like the design
    /// spike used — a bash builtin growing memory directly in its own
    /// process, no fork, needs no `rust:*` image). Proves the whole real
    /// path end to end: a genuine kernel OOM kill, Kubernetes actually
    /// reporting `OOMKilled`, and `pod_death_reason` extracting it from a
    /// *real* API response — the pure unit tests already cover the
    /// branching logic exhaustively, but only against constructed data.
    #[tokio::test]
    async fn test_pod_death_reason_reports_oomkilled_from_a_real_oom_kill() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("real-oom");
        // Small on purpose — fast and reliable to trigger, using this same
        // plan's own per-pod override rather than the real 8Gi default.
        let sandbox = manager.create(&session_id, "64Mi", "1").await.expect("create should succeed");
        let pods = pods_api(&client);

        // The whole `AttachedProcess` — not just its split-off stdout/
        // stderr handles — is moved into the spawned task, so the exec
        // session it owns stays open for as long as *that task* runs, not
        // tied to this function's own scope. Letting `exec` drop early
        // (e.g. a `{ ... }` block that only spawns readers off split
        // handles) closes the underlying connection, and since this
        // process is directly attached (not `setsid`-detached the way the
        // real agent injection is), the container runtime kills it right
        // along with the disconnect — the remote command never gets a
        // chance to actually run.
        if let Ok(mut exec) = pods
            .exec(&sandbox.pod_name, ["bash", "-c", "printf -v x '%*s' 200000000 ''; sleep 5"], &AttachParams::default())
            .await
        {
            tokio::spawn(async move {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut so) = exec.stdout() {
                    let _ = so.read_to_string(&mut out).await;
                }
                if let Some(mut se) = exec.stderr() {
                    let _ = se.read_to_string(&mut err).await;
                }
                exec.join().await.ok();
            });
        }

        let reason = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(reason) = pod_death_reason(&pods, &sandbox.pod_name).await {
                    return reason;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await
        .expect("pod should be confirmed dead within 30s of a real OOM trigger");

        assert_eq!(
            reason,
            Some("OOMKilled".to_string()),
            "a real OOM kill should surface as OOMKilled via the real Kubernetes API, not a constructed one"
        );

        std::mem::forget(sandbox);
    }

    #[tokio::test]
    async fn test_create_reuses_existing_running_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("reuse");

        let first = manager.create(&session_id, "8Gi", "1").await.expect("first create should succeed");
        let second = manager.create(&session_id, "8Gi", "1").await.expect("second create should reuse, not error");

        assert_eq!(first.pod_name, second.pod_name);

        let pods = pods_api(&client);
        pods.delete(&first.pod_name, &immediate_delete_params()).await.ok();
        std::mem::forget(first);
        std::mem::forget(second);
    }

    #[tokio::test]
    async fn test_exec_captures_stdout_and_exit_code() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("exec");
        let sandbox = manager.create(&session_id, "8Gi", "1").await.expect("create should succeed");

        let result = sandbox.exec(&["echo", "hello"]).await.expect("exec should succeed");
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);

        let failing = sandbox.exec(&["bash", "-c", "exit 7"]).await.expect("exec should succeed even for nonzero exit");
        assert_eq!(failing.exit_code, 7);

        let pods = pods_api(&client);
        pods.delete(&sandbox.pod_name, &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);
    }

    /// Characterization test for `pod_death_reason`'s real fetch, not
    /// `decide_pod_death_reason`'s branching (already exhaustively covered
    /// without a cluster) — proves the wrapper actually calls through to a
    /// live pod correctly in both directions: inconclusive while it's
    /// genuinely running, confirmed (with no reason to report) once it's
    /// gone entirely.
    #[tokio::test]
    async fn test_pod_death_reason_reflects_a_real_pod_then_its_absence() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("death-reason");
        let sandbox = manager.create(&session_id, "8Gi", "1").await.expect("create should succeed");
        let pods = pods_api(&client);

        assert_eq!(pod_death_reason(&pods, &sandbox.pod_name).await, None, "a genuinely Running pod is inconclusive");

        pods.delete(&sandbox.pod_name, &immediate_delete_params()).await.expect("delete should succeed");
        assert_eq!(
            pod_death_reason(&pods, &sandbox.pod_name).await,
            Some(None),
            "a pod that's gone entirely is confirmed dead with no reason to report"
        );
        std::mem::forget(sandbox);
    }

    // No standalone `force_terminate_pod` test: it's a private helper only
    // reachable through `terminate_pod`, which `test_terminal_lifecycle_end_to_end`
    // already exercises six times over. A second test independently racing
    // to set the process-global `MANAGER` singleton is actively harmful,
    // not just redundant — `kube::Client`'s internals are tied to whichever
    // tokio runtime first constructed it, and `#[sqlx::test]`/`#[tokio::test]`
    // each get their own runtime; whichever test's runtime tears down first
    // kills the shared client for every other test still relying on it via
    // `get()`. Confirmed live: adding this test back made
    // `test_terminal_lifecycle_end_to_end` fail with `Kube(Service(Closed))`
    // every time it ran after this one, even though neither test touches
    // the other's data.

    #[tokio::test]
    async fn test_manager_delete_removes_the_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("delete");
        let sandbox = manager.create(&session_id, "8Gi", "1").await.expect("create should succeed");
        let pod_name = sandbox.pod_name.clone();

        manager.delete(sandbox).await.expect("delete should succeed");

        let pods = pods_api(&client);
        let still_there = pods.get_opt(&pod_name).await.expect("get_opt should not error");
        assert!(still_there.is_none(), "pod should be gone immediately after manager.delete returns Ok");
    }

    #[tokio::test]
    async fn test_dropping_without_delete_still_cleans_up_via_drain_task() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("drop");
        let sandbox = manager.create(&session_id, "8Gi", "1").await.expect("create should succeed");
        let pod_name = sandbox.pod_name.clone();

        drop(sandbox); // no manager.delete call — this is the path under test

        let pods = pods_api(&client);
        let gone = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if pods.get_opt(&pod_name).await.expect("get_opt should not error").is_none() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "drain task should have deleted the pod within the timeout");
    }

}
