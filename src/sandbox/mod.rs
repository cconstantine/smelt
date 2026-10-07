//! Sandbox lifecycle: a disposable Kubernetes Pod per conversation, in the
//! `smelt-park` namespace, plus a persistent terminal reached through a
//! purpose-built agent that *is* the pod's own `ENTRYPOINT` (its real PID
//! 1 — see SME-17, built on a
//! custom image `scripts/build-sandbox-image.sh` produces and delivers
//! with no registry involved). Pod and terminal are separate,
//! explicitly-managed lifecycles — see
//! SME-9 for that design; the original
//! pod-only mechanism is SME-7.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
#[cfg(test)]
use futures_util::FutureExt;
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, ExecAction, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PersistentVolumeClaimVolumeSource, Pod, PodSpec, Probe, ResourceRequirements, SecurityContext,
    Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, AttachParams, DeleteParams, ListParams, PostParams};
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::docker_net::{DOCKER_BRIDGE_IP, DOCKER_NETWORK_POOL};
use crate::{db, events};

mod agent;
mod claims;
mod exec;
mod files;
mod manager;
mod ports;
mod spec;
mod watch;

pub use self::agent::*;
pub use self::claims::*;
pub use self::exec::*;
pub use self::files::*;
pub use self::manager::*;
pub use self::ports::*;
pub use self::spec::*;
pub use self::watch::*;

// `cfg(test)` rather than an env var deliberately: the whole point is that
// there's no way to forget to isolate a test run from `dx serve`'s real
// dev-instance pods — an env var can simply not be set. The `park`
// ServiceAccount (see k8s/smelt-park-rbac.yaml) is scoped to both
// namespaces via two separate Role/RoleBinding pairs, so the one
// `KUBECONFIG`/token already in use keeps working unchanged for either.
#[cfg(not(test))]
const NAMESPACE: &str = "smelt-park";
#[cfg(test)]
const NAMESPACE: &str = "smelt-park-test";
/// At least the Docker sidecar's startup probe, plus time for a first
/// pod's claims to be provisioned (SME-62 B15): 30 s gave up on pods that
/// would have started under load.
const DEFAULT_RUNNING_WAIT_TIMEOUT_SECS: u64 = 90;

/// The Docker sidecar's startup probe: how many checks, how far apart.
const DOCKER_STARTUP_PROBE_CHECKS: i32 = 60;
const DOCKER_STARTUP_PROBE_PERIOD_SECS: i32 = 1;

/// Matches `sandbox_agent`'s own `LISTEN_ADDR` port.
const AGENT_PORT: u16 = 8088;
/// The agent's relay into the pod's Docker networks (`RELAY_ADDR` in
/// src/bin/sandbox_agent.rs), SME-33.
const RELAY_PORT: u16 = 8089;
/// Longer than the relay's own 5s connect timeout, so its answer, or its
/// giving up, arrives first.
const RELAY_CONNECT_WAIT: Duration = Duration::from_secs(7);

#[derive(Debug)]
pub enum SandboxError {
    Kube(kube::Error),
    /// The pod didn't reach `Running` within `running_wait_timeout()`; the
    /// detail is the pod's own explanation, when it gave one.
    Timeout(Option<String>),
    /// A pod already existed for this session but wasn't `Running` (e.g.
    /// `Terminating`, `Failed`). What to do here is an open question in
    /// SME-7's plan — not resolved, just surfaced rather than guessed at.
    ExistingPodNotRunning(String),
    /// A local I/O failure reading or writing an exec's or a
    /// port-forward's stream.
    Io(std::io::Error),
    WebSocket(tokio_tungstenite::tungstenite::Error),
    /// A `sandbox_pods`/`sandbox_terminals` query failed — see SME-9's
    /// "How" on why pod/terminal identity is DB-backed this round.
    Db(sqlx::Error),
    /// `create_pod` refuses: this conversation already has a live pod. See
    /// SME-11's "One pod per conversation."
    PodAlreadyExists,
    InvalidMountPath(String),
    StartFailed(String),
    /// Writing the SSH keys and git config into a pod failed (SME-32).
    GitSetup(String),
    /// Reaching the sandbox agent (port-forward, WebSocket handshake and
    /// hello) took longer than `AGENT_CONNECT_TIMEOUT`.
    AgentConnectTimeout,
    /// A sandbox call before `init()` built the manager (SME-56).
    NotInitialized,
    /// kube gave no stream for something smelt asked it for: an exec's
    /// stdin, stdout or stderr, or a port-forward's port. kube returns one
    /// for every stream requested, so this is a kube upgrade changing that,
    /// failing the one request rather than panicking (SME-95).
    NoStream(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Kube(e) => write!(f, "kubernetes API error: {e}"),
            SandboxError::AgentConnectTimeout => write!(
                f,
                "timed out connecting to the sandbox agent after {}s",
                AGENT_CONNECT_TIMEOUT.as_secs()
            ),
            SandboxError::Timeout(None) => write!(f, "timed out waiting for pod to become Running"),
            SandboxError::Timeout(Some(detail)) => {
                write!(f, "timed out waiting for pod to become Running ({detail})")
            }
            SandboxError::ExistingPodNotRunning(phase) => {
                write!(f, "existing sandbox pod is not Running (phase: {phase})")
            }
            SandboxError::Io(e) => write!(f, "I/O error on an exec's or port-forward's stream: {e}"),
            SandboxError::WebSocket(e) => {
                write!(f, "WebSocket error talking to sandbox agent: {e}")
            }
            SandboxError::Db(e) => write!(f, "database error: {e}"),
            SandboxError::PodAlreadyExists => {
                write!(
                    f,
                    "a pod already exists for this conversation; call terminate_pod first"
                )
            }
            SandboxError::InvalidMountPath(reason) => write!(f, "invalid mount path: {reason}"),
            SandboxError::StartFailed(reason) => write!(f, "sandbox pod failed to start: {reason}"),
            SandboxError::GitSetup(reason) => write!(f, "couldn't set up git in the pod: {reason}"),
            SandboxError::NotInitialized => {
                write!(f, "the sandbox isn't set up yet (sandbox::init() hasn't run)")
            }
            SandboxError::NoStream(what) => write!(f, "kube gave no stream for {what}"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<kube::Error> for SandboxError {
    fn from(e: kube::Error) -> Self {
        SandboxError::Kube(e)
    }
}

/// The stream kube gave for `what`, or `SandboxError::NoStream` naming it.
pub(super) fn require_stream<T>(stream: Option<T>, what: &str) -> Result<T, SandboxError> {
    stream.ok_or_else(|| SandboxError::NoStream(what.to_owned()))
}

pub(crate) fn pods_api(client: &kube::Client) -> Api<Pod> {
    Api::namespaced(client.clone(), NAMESPACE)
}

fn pvc_api(client: &kube::Client) -> Api<PersistentVolumeClaim> {
    Api::namespaced(client.clone(), NAMESPACE)
}

// --- Terminal: pod, terminal, and command are three separate, explicitly
// guarded lifecycles, and a conversation may have N pods each with N
// terminals. See SME-9's "What" and "How". ---

#[derive(Debug)]
pub enum TerminalError {
    Sandbox(SandboxError),
    /// A call referencing a `pod_id` that doesn't exist (never created,
    /// already terminated, or not `Running`).
    NoPod,
    /// A call referencing a `terminal_id` that doesn't exist.
    NoTerminal,
    /// `terminate_pod` refuses while that pod still has a live terminal.
    TerminalStillExists,
    /// `terminate_terminal` refuses while a command is still `running` in
    /// that terminal.
    CommandStillRunning,
    /// A request to the pod's agent failed: it refused (its own words,
    /// which the model needs to see and act on), didn't answer, or the
    /// connection dropped.
    Agent(AgentRequestError),
    /// The pod's agent speaks another major version of the protocol, or
    /// predates versioning (`found: None`). Only a new pod fixes it.
    AgentOutdated { found: Option<ProtocolVersion> },
    /// A feature needs a newer minor version than the pod's agent has.
    AgentTooOld { needs: ProtocolVersion, found: ProtocolVersion },
    /// Every attempt to reach the pod's agent failed, so its pod was
    /// cleaned up and stopped, as after a crash.
    AgentUnreachable,
    /// A request to open one of the sandbox agent's own ports (`AGENT_PORT`,
    /// `RELAY_PORT`) from a route into the pod — see `check_reachable_port`.
    AgentPort(u16),
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TerminalError::Sandbox(e) => write!(f, "{e}"),
            TerminalError::NoPod => {
                write!(
                    f,
                    "no such pod (it doesn't exist, isn't Running, or was already terminated)"
                )
            }
            TerminalError::NoTerminal => write!(f, "no such terminal"),
            TerminalError::TerminalStillExists => {
                write!(
                    f,
                    "this pod still has a live terminal; call terminate_terminal on it first"
                )
            }
            TerminalError::CommandStillRunning => {
                write!(
                    f,
                    "a command is still running in this terminal; send_signal or wait for it to finish first"
                )
            }
            TerminalError::Agent(e) => write!(f, "{e}"),
            TerminalError::AgentOutdated { found } => {
                let found = match found {
                    Some(version) => format!("protocol {version}"),
                    None => "an agent from before the protocol was versioned".to_string(),
                };
                write!(
                    f,
                    "this sandbox runs an incompatible sandbox agent ({found}; smelt needs protocol {}.x), \
                     so its terminals and file tools can't be used. Call terminate_pod, then create_pod, \
                     for a sandbox that works; /workspace is kept.",
                    PROTOCOL_VERSION.major
                )
            }
            TerminalError::AgentTooOld { needs, found } => write!(
                f,
                "this needs a newer sandbox agent (protocol {needs} or later; this sandbox has {found}). \
                 Call terminate_pod, then create_pod, to get one; /workspace is kept."
            ),
            TerminalError::AgentUnreachable => write!(
                f,
                "the sandbox's agent couldn't be reached, so its pod was stopped (a notice says why, if \
                 the cluster gave a reason). Call create_pod for a new one."
            ),
            TerminalError::AgentPort(port) => write!(
                f,
                "port {port} is smelt's own sandbox agent, which can't be opened from a \
                 browser or a preview; run your server on another port"
            ),
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
    /// See `agent_status`.
    pub agent: Option<crate::api::pods::AgentStatus>,
}

#[derive(Debug)]
pub struct TerminalInfo {
    pub terminal_id: i64,
    pub pod_id: i64,
    pub status: String,
}

// --- The sandbox agent's connection. The wire types are shared with the
// agent in `agent_protocol` (SME-53). ---

pub use crate::agent_protocol::{DirEntry, FileContents, GlobResult, GrepResult};
use crate::agent_protocol::{AgentMessage, ClientMessage, PROTOCOL_VERSION, ProtocolVersion, Reply};

/// Why one request to a pod's agent failed.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRequestError {
    /// No answer within `ACK_TIMEOUT`.
    Timeout,
    /// The connection ended, or had already ended, before the answer came.
    Disconnected,
    /// The agent refused, in words meant for the model: a hash mismatch, an
    /// ambiguous edit, an unknown terminal.
    Rejected(String),
    /// The agent answered with a kind of reply that doesn't fit the request.
    UnexpectedReply(&'static str),
    /// smelt couldn't turn its own request into JSON.
    Unencodable(String),
}

impl std::fmt::Display for AgentRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentRequestError::Timeout => write!(
                f,
                "the sandbox agent didn't answer within {}s; the pod may be overloaded. Try again.",
                ACK_TIMEOUT.as_secs()
            ),
            AgentRequestError::Disconnected => write!(
                f,
                "the connection to the sandbox agent dropped during the request. Try again; if the pod \
                 stopped, you'll be told separately."
            ),
            AgentRequestError::Rejected(message) => write!(f, "{message}"),
            AgentRequestError::UnexpectedReply(action) => write!(
                f,
                "the sandbox agent answered {action} with a different kind of reply (a smelt bug)"
            ),
            AgentRequestError::Unencodable(e) => {
                write!(f, "smelt couldn't encode its request to the sandbox agent ({e}; a smelt bug)")
            }
        }
    }
}

impl From<AgentRequestError> for TerminalError {
    fn from(e: AgentRequestError) -> Self {
        TerminalError::Agent(e)
    }
}

#[cfg(test)]
mod tests;

/// smelt's side of the agent connection against a fake agent on a loopback
/// port (SME-53): requests and their errors, the hello, and one connection
/// per pod. The real agent and cluster are `test_terminal_lifecycle_end_to_end`'s.
#[cfg(test)]
mod agent_connection_tests;
