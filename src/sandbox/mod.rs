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
    /// A local I/O failure: `Sandbox::exec`'s (real-cluster tests only), or
    /// a port-forward that gave no stream for the agent's port.
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
            SandboxError::Io(e) => write!(f, "I/O error reading exec output: {e}"),
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
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<kube::Error> for SandboxError {
    fn from(e: kube::Error) -> Self {
        SandboxError::Kube(e)
    }
}

pub(crate) fn pods_api(client: &kube::Client) -> Api<Pod> {
    Api::namespaced(client.clone(), NAMESPACE)
}

fn pvc_api(client: &kube::Client) -> Api<PersistentVolumeClaim> {
    Api::namespaced(client.clone(), NAMESPACE)
}

fn pod_name(pod_id: i64) -> String {
    format!("sandbox-{pod_id}")
}

/// The sandbox user's home directory — fixed by Phase 1's `useradd -m
/// sandbox` in `docker/sandbox/Dockerfile`, not derived from anything at
/// runtime.
const SANDBOX_HOME: &str = "/home/sandbox";

/// Expands a leading `~` in a user-typed mount path to the sandbox user's
/// home directory — smelt's own expansion, not something deferred to the
/// pod spec (Kubernetes' `volumeMounts.mountPath` has no concept of `~`).
/// Only a bare `~` or a `~/...` prefix expands, matching ordinary shell
/// semantics for the single-user case (no `~otheruser` support — there's
/// only ever one sandbox user). Anything else passes through unchanged.
/// A volume's mount path is mounted into every sandbox pod, and a container
/// can't start with a relative (or root, or `..`-escaping) mount
/// destination — one bad volume would stop every new sandbox. Checked after
/// `resolve_mount_path` expands `~`.
fn validate_mount_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("{path:?} must be an absolute path (start with / or ~)"));
    }
    if path == "/" {
        return Err("a volume can't be mounted over the root directory".to_string());
    }
    if path.split('/').any(|part| part == "..") {
        return Err(format!("{path:?} must not contain .."));
    }
    Ok(())
}

/// Creates any volume claim that's missing from this namespace. Every pod
/// mounts every configured volume, and a pod whose claim doesn't exist
/// can never be scheduled — it just waits until the start-up timeout. A
/// claim goes missing when the cluster is rebuilt or the claim deleted,
/// or when the volume was created in another namespace (the browser tier
/// shares the dev database but runs in the test namespace). Whatever the
/// old claim held is gone either way; recreating it keeps sandboxes working.
async fn ensure_volume_claims(
    client: &kube::Client,
    volumes: &[db::SandboxVolume],
) -> Result<(), SandboxError> {
    let pvcs = pvc_api(client);
    for volume in volumes {
        let claim = sandbox_volume_pvc_name(volume.id);
        if pvcs.get_opt(&claim).await?.is_none() {
            tracing::warn!(claim = %claim, "volume claim missing; recreating it");
            pvcs.create(&PostParams::default(), &build_volume_pvc_spec(volume.id))
                .await?;
        }
    }
    Ok(())
}

/// Why `pod` isn't running yet, from its first failing condition — e.g.
/// "PodScheduled: Unschedulable: 0/1 nodes are available: persistentvolumeclaim
/// … not found." Reported when waiting times out, so the error says why.
fn pod_pending_detail(pod: &Pod) -> Option<String> {
    let conditions = pod.status.as_ref()?.conditions.as_ref()?;
    conditions.iter().find(|c| c.status == "False").map(|c| {
        let reason = c.reason.as_deref().unwrap_or("");
        let message = c.message.as_deref().unwrap_or("");
        [c.type_.as_str(), reason, message]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(": ")
    })
}

/// Container waiting states that never resolve on their own — once a pod's
/// container is in one of these, waiting for `Running` only ends in a
/// timeout that hides the reason.
const FATAL_WAITING_REASONS: [&str; 7] = [
    "CreateContainerError",
    "CreateContainerConfigError",
    "RunContainerError",
    "ErrImagePull",
    "ImagePullBackOff",
    "InvalidImageName",
    "CrashLoopBackOff",
];

/// Why `pod` can't start, if one of its containers is stuck in a state that
/// won't recover (see `FATAL_WAITING_REASONS`); `None` while it's still
/// starting normally.
fn pod_startup_failure(pod: &Pod) -> Option<String> {
    let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
    statuses.iter().find_map(|status| {
        let waiting = status.state.as_ref()?.waiting.as_ref()?;
        let reason = waiting.reason.as_deref()?;
        FATAL_WAITING_REASONS.contains(&reason).then(|| {
            match waiting.message.as_deref().filter(|m| !m.is_empty()) {
                Some(message) => format!("{reason}: {message}"),
                None => reason.to_string(),
            }
        })
    })
}

fn resolve_mount_path(path: &str) -> String {
    if path == "~" {
        SANDBOX_HOME.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{SANDBOX_HOME}/{rest}")
    } else {
        path.to_string()
    }
}

/// The PVC name backing a given `sandbox_volumes` row — deterministic,
/// same `sandbox-{id}` naming convention `pod_name` already uses.
fn sandbox_volume_pvc_name(volume_id: i64) -> String {
    format!("sandbox-volume-{volume_id}")
}

/// Every configured volume's `Volume`/`VolumeMount` pair for a pod spec —
/// pulled out as a pure function over an already-fetched list, same
/// testability reason as `check_pod_guard`/`resolve_pod_id`. Every pod
/// gets every volume, unconditionally — see the idea's "not something
/// chosen per conversation."
fn volume_mounts_for(volumes: &[db::SandboxVolume]) -> (Vec<Volume>, Vec<VolumeMount>) {
    volumes
        .iter()
        .map(|v| {
            let name = format!("volume-{}", v.id);
            let volume = Volume {
                name: name.clone(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: sandbox_volume_pvc_name(v.id),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mount = VolumeMount {
                name,
                mount_path: v.mount_path.clone(),
                ..Default::default()
            };
            (volume, mount)
        })
        .unzip()
}

/// A sandbox pod is disposable — there's nothing inside it worth a graceful
/// in-process shutdown, so deletes skip Kubernetes' default (per-pod,
/// commonly 30s) grace period rather than waiting on it. Verified this
/// matters in practice: a plain `sleep infinity` container doesn't trap
/// `SIGTERM`, so a default-grace-period delete leaves the pod `Terminating`
/// for the full grace period before it actually disappears.
/// How long a deleted pod's containers get to stop. dockerd needs it to stop
/// its containers (its own shutdown timeout is 15s), and until then
/// Kubernetes keeps listing the pod, which is what lets the next `create_pod`
/// wait for it rather than mount the Docker claim alongside a dockerd that's
/// still writing to it (SME-51 B6).
const POD_DELETE_GRACE_SECS: u32 = 20;

/// Every delete of a sandbox pod smelt makes.
pub(crate) fn pod_delete_params() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(POD_DELETE_GRACE_SECS),
        ..Default::default()
    }
}

/// Tests' own cleanup, which needs nothing to stop cleanly.
#[cfg(test)]
fn immediate_delete_params() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    }
}

pub struct Sandbox {
    pod_name: String,
    // Only read by `exec` below, which is itself real-cluster-test-only
    // (see its own cfg) — production code talks to the sandbox through
    // `sandbox_agent`'s WebSocket protocol instead.
    #[cfg_attr(not(test), allow(dead_code))]
    client: kube::Client,
    cleanup_tx: mpsc::UnboundedSender<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl Sandbox {
    /// Lower-level kube-exec path used only by the real-cluster tests below
    /// to check a raw property of the pod itself (its mounted volumes, its
    /// non-root user, ...) independent of `sandbox_agent`'s own WebSocket
    /// terminal protocol, which is what production code actually uses.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn exec(&self, command: &[&str]) -> Result<ExecResult, SandboxError> {
        self.exec_in("sandbox", command).await
    }

    /// `exec` in a named container of the pod — `"docker"` for the Docker
    /// sidecar (SME-33).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn exec_in(
        &self,
        container: &str,
        command: &[&str],
    ) -> Result<ExecResult, SandboxError> {
        exec_with(&self.client, &self.pod_name, container, command, None).await
    }
}

/// kube exec in `pod_name`'s `container`. `stdin`, when given, is written
/// and then closed, so a command like `cat > file` sees its end. kube
/// closes just the stdin stream on a v5 connection (k3s has it); an older
/// server closes the whole connection and the exit code comes back
/// missing, which callers treat as a failure.
pub(crate) async fn exec_with(
    client: &kube::Client,
    pod_name: &str,
    container: &str,
    command: &[&str],
    stdin: Option<&[u8]>,
) -> Result<ExecResult, SandboxError> {
    let pods = pods_api(client);
    let mut attached = pods
        .exec(
            pod_name,
            command.iter().copied(),
            &AttachParams::default()
                .container(container)
                .stdin(stdin.is_some()),
        )
        .await?;
    if let Some(input) = stdin {
        use tokio::io::AsyncWriteExt;
        let mut writer = attached.stdin().expect("stdin requested above");
        writer.write_all(input).await.map_err(SandboxError::Io)?;
        writer.shutdown().await.map_err(SandboxError::Io)?;
        drop(writer);
    }

    let mut stdout_reader = attached
        .stdout()
        .expect("stdout requested by AttachParams::default()");
    let mut stderr_reader = attached
        .stderr()
        .expect("stderr requested by AttachParams::default()");
    // Bytes, decoded leniently: a file's contents need not be UTF-8, and
    // a `head -c` cut can split a character (SME-32).
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let (stdout_res, stderr_res) = tokio::join!(
        stdout_reader.read_to_end(&mut stdout),
        stderr_reader.read_to_end(&mut stderr),
    );
    stdout_res.map_err(SandboxError::Io)?;
    stderr_res.map_err(SandboxError::Io)?;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

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

/// Writes git's files (`git::pod_git_files`) into a pod's sandbox
/// container. The keys directory is replaced wholesale, so a key deleted
/// since the last install goes too.
pub async fn install_git_files(
    client: &kube::Client,
    pod_name: &str,
    files: &[crate::git::PodFile],
) -> Result<(), SandboxError> {
    // Never emptied: a clone or push running during a reinstall must
    // still find its key. Keys are written over in place, and only the
    // ones no longer in `files` removed afterwards.
    let keys_dir = format!("{}/keys", crate::git::POD_GIT_DIR);
    let made = exec_with(
        client,
        pod_name,
        "sandbox",
        &["sh", "-c", r#"mkdir -p -m 700 "$1""#, "sh", &keys_dir],
        None,
    )
    .await?;
    if made.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't make {keys_dir}: {}",
            made.stderr.trim()
        )));
    }
    for file in files {
        // Written beside the target and renamed over it, so ssh or git
        // never reads half a file; umask keeps a key private from its
        // first byte.
        let mode = format!("{:o}", file.mode);
        let written = exec_with(
            client,
            pod_name,
            "sandbox",
            &[
                "sh",
                "-c",
                r#"umask 077 && cat > "$1.new" && chmod "$2" "$1.new" && mv "$1.new" "$1""#,
                "sh",
                &file.path,
                &mode,
            ],
            Some(file.content.as_bytes()),
        )
        .await?;
        if written.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't write {}: {}",
                file.path,
                written.stderr.trim()
            )));
        }
    }
    let keep: Vec<&str> = files
        .iter()
        .filter_map(|f| f.path.strip_prefix(&format!("{keys_dir}/")))
        .collect();
    let mut prune = vec![
        "sh",
        "-c",
        r#"cd "$1" && shift && for f in * .[!.]*; do
               [ -e "$f" ] || continue
               keep=; for k in "$@"; do [ "$f" = "$k" ] && keep=1; done
               [ -n "$keep" ] || rm -f -- "$f"
           done"#,
        "sh",
        &keys_dir,
    ];
    prune.extend(keep);
    let pruned = exec_with(client, pod_name, "sandbox", &prune, None).await?;
    if pruned.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't remove deleted keys from {keys_dir}: {}",
            pruned.stderr.trim()
        )));
    }
    Ok(())
}

/// The Kubernetes client every sandbox operation uses.
pub(crate) fn kube_client() -> kube::Client {
    get().client.clone()
}

/// `install_git_files` for a live pod of smelt's own, by id.
pub async fn install_git_files_in_pod(
    pod_id: i64,
    files: &[crate::git::PodFile],
) -> Result<(), SandboxError> {
    install_git_files(&get().client, &pod_name(pod_id), files).await
}

/// On success the exec protocol's terminal `Status` carries no exit code at
/// all (implying 0); on a non-zero exit it's a `StatusCause` with
/// `reason == "ExitCode"` and the code itself, as a string, in `message`.
/// Verified against a real cluster, not assumed — see SME-7's plan.
///
/// No status at all means the connection ended before the API server said
/// how the command did, so it counts as a failure (-1), not a success.
fn extract_exit_code(
    status: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>,
) -> i32 {
    let Some(status) = status else {
        return -1;
    };
    status
        .details
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

    /// `create_with_docker` for tests, which have no conversation: a small
    /// Docker sidecar whose data dies with the pod.
    #[cfg(test)]
    pub async fn create(
        &self,
        session_id: &str,
        memory: &str,
        volumes: &[db::SandboxVolume],
    ) -> Result<Sandbox, SandboxError> {
        let docker = DockerSidecar {
            memory: "512Mi".to_string(),
            storage: PodStorage::Ephemeral,
        };
        self.create_with_docker(session_id, memory, &docker, volumes).await
    }

    /// `memory` is an already-resolved value (the caller's own
    /// override, or its default) — only used on the actual-creation
    /// branch below; the reuse branch has nothing to apply them to, since
    /// resources are immutable on an already-existing pod. The same goes
    /// for `docker`, the Docker sidecar's limits and storage (SME-33).
    /// `volumes` is every currently-configured `sandbox_volumes` row —
    /// every pod gets every one of them mounted, unconditionally (see
    /// SME-17's Phase 4); an empty slice is fine for callers (mostly
    /// tests) that don't care.
    pub async fn create_with_docker(
        &self,
        session_id: &str,
        memory: &str,
        docker: &DockerSidecar,
        volumes: &[db::SandboxVolume],
    ) -> Result<Sandbox, SandboxError> {
        self.create_with_running_timeout(
            session_id,
            memory,
            docker,
            volumes,
            running_wait_timeout(),
        )
        .await
    }

    /// Split out of `create` so a test can exercise the "pod never reaches
    /// `Running`" path with a short, explicit timeout instead of either
    /// waiting out the real (30s+) default or mutating the
    /// `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` env var — a process-global that
    /// other tests read concurrently, so overriding it here would risk
    /// spuriously timing *them* out instead. `create` itself just forwards
    /// the real env-driven default.
    async fn create_with_running_timeout(
        &self,
        session_id: &str,
        memory: &str,
        docker: &DockerSidecar,
        volumes: &[db::SandboxVolume],
        running_timeout: Duration,
    ) -> Result<Sandbox, SandboxError> {
        let pods = pods_api(&self.client);
        let name = format!("sandbox-{session_id}");

        // Tracks whether *this call* created the pod, as opposed to
        // reusing one that already existed — only a pod we just created is
        // ours to delete if it never reaches `Running` below; a reused
        // pod's non-`Running` status could be a transient blip on
        // something another part of the system still depends on.
        let just_created = match pods.get_opt(&name).await? {
            Some(pod) => {
                let phase = pod.status.and_then(|s| s.phase).unwrap_or_default();
                if phase != "Running" {
                    // Non-Running existing pod (Terminating, Failed, ...)
                    // is still an open question per SME-7's plan — not
                    // handled yet.
                    return Err(SandboxError::ExistingPodNotRunning(phase));
                }
                // Reuse: what makes an active conversation's sandbox
                // survive a smelt server restart, see SME-7's "Restart
                // behavior" section.
                false
            }
            None => {
                ensure_volume_claims(&self.client, volumes).await?;
                pods.create(
                    &PostParams::default(),
                    &build_pod_spec(&name, memory, docker, volumes),
                )
                .await?;
                true
            }
        };

        if let Err(e) = wait_for_running_with_timeout(&pods, &name, running_timeout).await {
            if just_created {
                // Nothing else ever cleans this up: `Sandbox::drop`'s
                // cleanup-queue only fires for a `Sandbox` we actually
                // return, which never happens on this path. Left alone,
                // it sits forever — worse, a caller that derives this same
                // pod name deterministically (as `sandbox_volume_pvc_name`
                // does; see `test_terminal_lifecycle_end_to_end`'s own
                // precheck) collides with it on every subsequent attempt.
                pods.delete(&name, &pod_delete_params()).await.ok();
            }
            return Err(e);
        }

        Ok(Sandbox {
            pod_name: name,
            client: self.client.clone(),
            cleanup_tx: self.cleanup_tx.clone(),
        })
    }

    /// Only called by the tests below (production relies on `Sandbox`'s own
    /// `Drop` impl, which queues the same cleanup asynchronously) — kept as
    /// an explicit, synchronous alternative for tests that need to assert
    /// on the pod's absence immediately after deleting, without racing the
    /// cleanup queue.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn delete(&self, sandbox: Sandbox) -> Result<(), SandboxError> {
        let pods = pods_api(&self.client);
        pods.delete(&sandbox.pod_name, &pod_delete_params())
            .await?;
        // Disarms Drop: safe to skip since none of Sandbox's fields have
        // meaningful Drop side effects of their own (a String, a
        // cheaply-Clone/Arc-backed kube::Client, an UnboundedSender whose
        // Drop is just a refcount decrement) — see SME-7's plan.
        std::mem::forget(sandbox);
        Ok(())
    }
}

/// `SANDBOX_MEMORY_LIMIT`, default `"8Gi"` if unset or empty. The
/// *default* a pod gets when `create_pod`'s caller doesn't specify its own
/// `memory_limit` — see SME-12's "Per-pod limit overrides."
fn default_memory_limit() -> String {
    std::env::var("SANDBOX_MEMORY_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8Gi".to_string())
}

/// `SANDBOX_IMAGE`, default `"docker.io/library/smelt-sandbox:latest"` —
/// same pattern as `default_memory_limit`. The custom image
/// `scripts/build-sandbox-image.sh` builds and delivers with no registry
/// involved (see SME-17) — its
/// own `ENTRYPOINT` is the sandbox agent, which is what makes the agent
/// the pod's real PID 1 rather than something injected and launched after
/// the fact. The fully-qualified default (not just `smelt-sandbox:latest`)
/// matches exactly what `ctr images import` registers the image as —
/// confirmed by spike, not assumed.
fn default_sandbox_image() -> String {
    std::env::var("SANDBOX_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "docker.io/library/smelt-sandbox:latest".to_string())
}

/// `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS`, default `90` — same pattern as
/// `default_memory_limit`. How long `wait_for_running` waits for a pod to
/// reach `Running` before giving up with `SandboxError::Timeout`. The
/// homelab cluster's real scheduling latency is why this exists as a
/// tunable rather than a bare constant: a CPU-constrained CI runner
/// schedules pods measurably slower than a real cluster or a
/// resource-rich dev machine, so CI raises it further. See
/// `DEFAULT_RUNNING_WAIT_TIMEOUT_SECS` for why the default is 90, and
/// docs/setup.md.
fn running_wait_timeout() -> Duration {
    let secs = std::env::var("SANDBOX_RUNNING_WAIT_TIMEOUT_SECS")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_RUNNING_WAIT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Where a pod keeps what should outlive it: the Docker sidecar's
/// `/var/lib/docker` (SME-33) and `/workspace` (SME-32).
#[derive(Debug, Clone, PartialEq)]
pub enum PodStorage {
    /// The conversation's own PVCs (`sandbox-docker-<id>` and
    /// `sandbox-workspace-<id>`), so images, build cache and the
    /// conversation's files outlive the pod.
    Conversation(i64),
    /// emptyDirs that die with the pod, for tests, which have no
    /// conversation.
    #[cfg(test)]
    Ephemeral,
}

/// A pod's Docker sidecar: its own memory limit, which nested containers
/// count against, and where its data lives (SME-33).
#[derive(Debug, Clone)]
pub struct DockerSidecar {
    pub memory: String,
    pub storage: PodStorage,
}

/// `SANDBOX_DOCKER_MEMORY_LIMIT`, default `"8Gi"` — see
/// `default_memory_limit`. Nested containers count against it, not the
/// sandbox container's limit.
fn default_docker_memory_limit() -> String {
    std::env::var("SANDBOX_DOCKER_MEMORY_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8Gi".to_string())
}

/// Shared by the sandbox and Docker sidecar containers: projects that
/// bind-mount host paths go here, and terminals start here.
pub const WORKSPACE_DIR: &str = "/workspace";
const WORKSPACE_VOLUME: &str = "workspace";
/// Holds only dockerd's socket, never dockerd's own `/run/docker`: that
/// holds containerd's state and pid file, and a copy shared with the
/// sandbox survives a sidecar restart and makes the next dockerd fail
/// (SME-33's spike).
const DOCKER_SOCK_DIR: &str = "/run/docker-sock";
const DOCKER_SOCK_VOLUME: &str = "docker-sock";
const DOCKER_HOST: &str = "unix:///run/docker-sock/docker.sock";
const DOCKER_DATA_VOLUME: &str = "docker-data";
/// The `docker` group's GID in the sandbox image
/// (docker/sandbox/Dockerfile); dockerd gives it the socket.
const DOCKER_GID: u32 = 2375;
/// The sandbox user's uid:gid, pinned in the image
/// (docker/sandbox/Dockerfile); the Docker sidecar gives it /workspace.
const SANDBOX_OWNER: &str = "1000:1000";

/// The conversation's Docker data PVC, `sandbox-docker-<id>`.
fn docker_pvc_name(conversation_id: i64) -> String {
    format!("sandbox-docker-{conversation_id}")
}

/// The conversation's /workspace PVC, `sandbox-workspace-<id>` (SME-32).
pub(crate) fn workspace_pvc_name(conversation_id: i64) -> String {
    format!("sandbox-workspace-{conversation_id}")
}

/// Label naming a conversation, on its Docker data PVC and on every pod
/// that mounts that PVC.
const CONVERSATION_LABEL: &str = "smelt/conversation";

/// `SANDBOX_DOCKER_STORAGE_SIZE`, default `"20Gi"` — see
/// `default_memory_limit`. Each conversation's Docker data PVC requests
/// this much.
fn default_docker_storage_size() -> String {
    std::env::var("SANDBOX_DOCKER_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "20Gi".to_string())
}

/// `SANDBOX_WORKSPACE_STORAGE_SIZE`, default `"20Gi"` — see
/// `default_memory_limit`. Each conversation's /workspace PVC requests
/// this much.
fn default_workspace_storage_size() -> String {
    std::env::var("SANDBOX_WORKSPACE_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "20Gi".to_string())
}

/// A conversation's claims: its Docker data and its /workspace (SME-32),
/// both kept across its pods and deleted with it.
fn conversation_pvc_specs(conversation_id: i64) -> [PersistentVolumeClaim; 2] {
    [
        build_conversation_pvc_spec(
            docker_pvc_name(conversation_id),
            conversation_id,
            default_docker_storage_size(),
        ),
        build_conversation_pvc_spec(
            workspace_pvc_name(conversation_id),
            conversation_id,
            default_workspace_storage_size(),
        ),
    ]
}

fn build_conversation_pvc_spec(name: String, conversation_id: i64, size: String) -> PersistentVolumeClaim {
    let mut requests = std::collections::BTreeMap::new();
    requests.insert("storage".to_string(), Quantity(size));
    let mut labels = std::collections::BTreeMap::new();
    labels.insert(
        CONVERSATION_LABEL.to_string(),
        conversation_id.to_string(),
    );

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: Some(NAMESPACE.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The conversations whose Docker data claims outlived them: claims
/// labelled with a conversation that isn't in `live`.
fn orphaned_docker_claims(
    claims: &[PersistentVolumeClaim],
    live: &std::collections::HashSet<i64>,
) -> Vec<i64> {
    claims
        .iter()
        .filter_map(|c| c.metadata.labels.as_ref()?.get(CONVERSATION_LABEL)?.parse().ok())
        .filter(|id| !live.contains(id))
        .collect::<std::collections::BTreeSet<i64>>()
        .into_iter()
        .collect()
}

/// Deletes Docker data claims whose conversation is gone. Conversation
/// teardown deletes its claim best-effort, so this catches what that
/// missed. Run once at startup by `main`, never from tests: tests share
/// the namespace but each has its own database, so from a test every
/// other test's claim would look orphaned.
pub async fn sweep_orphaned_conversation_claims(pool: &PgPool) {
    let client = &get().client;
    let selector = ListParams::default().labels(CONVERSATION_LABEL);
    // Claims first, then conversations: a claim only exists for a
    // conversation that already did, so none can be missed in between.
    let claims = match pvc_api(client).list(&selector).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list docker data claims to sweep");
            return;
        }
    };
    let live = match db::list_conversations(pool).await {
        Ok(conversations) => conversations.into_iter().map(|c| c.id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list conversations to sweep docker data claims");
            return;
        }
    };
    for conversation_id in orphaned_docker_claims(&claims, &live) {
        tracing::info!(conversation_id, "deleting a deleted conversation's docker data claim");
        delete_conversation_pvcs(client, conversation_id).await;
    }
}

/// The pod's Docker sidecar's status, if it has one yet.
fn docker_status(pod: &Pod) -> Option<&k8s_openapi::api::core::v1::ContainerStatus> {
    pod.status
        .as_ref()?
        .init_container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == "docker")
}

/// Whether the pod's Docker sidecar restarted since `last_reported`
/// restarts, and if so its new restart count and the reason Kubernetes
/// gave for the last exit (e.g. `OOMKilled`).
fn docker_restart_to_report(pod: &Pod, last_reported: i32) -> Option<(i32, Option<String>)> {
    let docker = docker_status(pod)?;
    if docker.restart_count <= last_reported {
        return None;
    }
    let reason = docker
        .last_state
        .as_ref()
        .and_then(|s| s.terminated.as_ref())
        .and_then(|t| t.reason.clone());
    Some((docker.restart_count, reason))
}

/// What `watch_pods` knows of a pod's Docker sidecar: the restart count
/// it last saw, and the reason of the last exit it saw (`state.terminated`
/// in the update before a restart; SME-85).
#[derive(Debug, Default, Clone, PartialEq)]
struct DockerSeen {
    restarts: i32,
    exit_reason: Option<String>,
}

/// A Docker sidecar restart for `watch_pods` to report.
#[derive(Debug, PartialEq)]
enum DockerRestart {
    /// Report it now, with this reason.
    Report { pod_id: i64, reason: String },
    /// No update has said why yet: re-read the pod after
    /// `DOCKER_REASON_WAIT` and report it then.
    Recheck { pod_id: i64 },
}

/// `watch_pods`' bookkeeping for Docker sidecar restarts: remembers each
/// pod's restart count and last exit reason in `seen`, and returns a
/// restart to report. A pod first seen in a listing (`initial`, after
/// smelt starts or the watch reconnects) is only remembered: there's
/// nothing to compare its count with. A known pod's re-listing is compared
/// like any update.
fn note_docker_restarts(
    seen: &mut HashMap<i64, DockerSeen>,
    pod: &Pod,
    initial: bool,
) -> Option<DockerRestart> {
    let pod_id = watched_pod_id(pod)?;
    // In a listing, a pod already known is one re-listed after the watch
    // reconnected: a restart since is one missed while disconnected (SME-88).
    let known = seen.contains_key(&pod_id);
    let entry = seen.entry(pod_id).or_default();
    let exited = docker_status(pod)
        .and_then(|c| c.state.as_ref()?.terminated.as_ref()?.reason.clone());
    if exited.is_some() {
        entry.exit_reason = exited;
    }
    let (count, reason) = docker_restart_to_report(pod, entry.restarts)?;
    entry.restarts = count;
    if initial && !known {
        // A starting point, not a restart: an exit it shows explains the
        // restart to come, so it's kept (SME-85's second code review).
        return None;
    }
    // A finished pod's containers are gone with it: crash cleanup reports
    // that, not a Docker restart (SME-88 code review).
    if pod_has_finished(pod) {
        return None;
    }
    // The exit seen before this restart explains only this one.
    let remembered = entry.exit_reason.take();
    Some(match reason.or(remembered) {
        Some(reason) => DockerRestart::Report { pod_id, reason },
        None => DockerRestart::Recheck { pod_id },
    })
}

/// Forgets the Docker sidecars of pods a re-listing didn't include: they
/// were deleted while the watch was disconnected, so no `Delete` came.
fn forget_unlisted_docker(seen: &mut HashMap<i64, DockerSeen>, listed: &std::collections::HashSet<i64>) {
    seen.retain(|pod_id, _| listed.contains(pod_id));
}

/// How long `watch_pods` waits before re-reading a pod whose Docker
/// sidecar restarted with no reason given yet (SME-85).
const DOCKER_REASON_WAIT: Duration = Duration::from_secs(3);

/// Reports `pod_id`'s Docker restart after `DOCKER_REASON_WAIT`, with the
/// reason the pod's status gives by then, if any.
fn recheck_docker_restart(pool: PgPool, client: kube::Client, pod_id: i64) {
    tokio::spawn(async move {
        tokio::time::sleep(DOCKER_REASON_WAIT).await;
        let reason = match pods_api(&client).get_opt(&pod_name(pod_id)).await {
            Ok(Some(pod)) => docker_restart_to_report(&pod, 0).and_then(|(_, reason)| reason),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(pod_id, error = %e, "couldn't re-read a pod whose Docker restarted");
                None
            }
        };
        report_docker_restart(&pool, pod_id, reason).await;
    });
}

/// Reports `restart` now, or after a re-read when no update said why.
async fn handle_docker_restart(pool: &PgPool, restart: DockerRestart) {
    match restart {
        DockerRestart::Report { pod_id, reason } => report_docker_restart(pool, pod_id, Some(reason)).await,
        DockerRestart::Recheck { pod_id } => recheck_docker_restart(pool.clone(), get().client.clone(), pod_id),
    }
}

/// Tells the pod's conversation that its Docker sidecar restarted, then
/// wakes it, the same way a crashed pod is reported (see
/// `handle_crash_cleanup`): the containers it ran are gone, which a
/// command waiting on one of them may need to hear about.
async fn report_docker_restart(pool: &PgPool, pod_id: i64, reason: Option<String>) {
    let conversation_id = match db::sandbox_pod_conversation_id(pool, pod_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't find the conversation of a pod whose Docker restarted");
            return;
        }
    };
    let why = reason.map(|r| format!(" ({r})")).unwrap_or_default();
    let notice = format!(
        "Docker in sandbox pod {pod_id} was restarted{why}, so the containers it was running \
         have stopped. Images, terminals and /workspace are unaffected. Every container shares \
         Docker's memory limit; a pod created with a larger docker_memory_limit gives them more."
    );
    let pool = pool.clone();
    tokio::spawn(async move {
        crate::api::chat::deliver_notice(&pool, conversation_id, notice).await;
    });
}

/// Waits until no pod labelled with `conversation_id` is left in the
/// cluster, so a new pod never shares the Docker claim with one still
/// stopping.
async fn wait_for_conversation_pods_gone(
    client: &kube::Client,
    conversation_id: i64,
    timeout: Duration,
) -> Result<(), SandboxError> {
    let pods = pods_api(client);
    let selector = ListParams::default().labels(&format!("{CONVERSATION_LABEL}={conversation_id}"));
    let waited = tokio::time::timeout(timeout, async {
        loop {
            let remaining = pods.list(&selector).await?;
            if remaining.items.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    waited.map_err(|_| {
        SandboxError::Timeout(Some(
            "the conversation's previous pod is still stopping".to_string(),
        ))
    })?
}

/// Creates the conversation's claims unless they already exist, so its
/// later pods reuse its images and build cache and find /workspace as the
/// last pod left it.
async fn ensure_conversation_pvcs(client: &kube::Client, conversation_id: i64) -> Result<(), SandboxError> {
    let pvcs = pvc_api(client);
    for spec in conversation_pvc_specs(conversation_id) {
        let name = spec.metadata.name.clone().expect("named above");
        if pvcs.get_opt(&name).await?.is_some() {
            continue;
        }
        match pvcs.create(&PostParams::default(), &spec).await {
            // Another create for the same conversation got there first.
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            other => {
                other?;
            }
        }
    }
    Ok(())
}

/// Best-effort, like the rest of conversation teardown: logged, never
/// returned. The startup sweep catches whatever this misses.
async fn delete_conversation_pvcs(client: &kube::Client, conversation_id: i64) {
    for name in [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)] {
        match pvc_api(client).delete(&name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => tracing::warn!(claim = %name, error = %e, "failed to delete a conversation's claim"),
        }
    }
}

/// `SANDBOX_DOCKER_IMAGE`, default `"docker.io/library/docker:29-dind"` —
/// see `default_sandbox_image`. Delivered into the node like the sandbox
/// image, and only its `dockerd` is used: never its entrypoint.
fn default_docker_image() -> String {
    std::env::var("SANDBOX_DOCKER_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "docker.io/library/docker:29-dind".to_string())
}

/// Starts dockerd inside the sidecar's own cgroup instead of through the
/// docker:dind image's entrypoint; see the script's own comment.
const START_DOCKERD_SCRIPT: &str = include_str!("../../docker/sandbox/start-dockerd.sh");

/// `memory` is a plain Kubernetes `Quantity` string (`"8Gi"`)
/// — no app-side parsing or validation of the format; an invalid value is
/// rejected by the Kubernetes API itself when the pod is actually
/// created, surfacing back through `SandboxError::Kube` as an ordinary
/// error. Bounded from above by the `smelt-park` namespace's own
/// `LimitRange` (`k8s/smelt-park-rbac.yaml`), not by anything here.
fn build_pod_spec(
    name: &str,
    memory: &str,
    docker: &DockerSidecar,
    volumes: &[db::SandboxVolume],
) -> Pod {
    let (mut pod_volumes, user_mounts) = volume_mounts_for(volumes);
    pod_volumes.extend([
        workspace_volume(&docker.storage),
        empty_dir_volume(DOCKER_SOCK_VOLUME),
        docker_data_volume(&docker.storage),
    ]);
    // Both containers see the same files at the same paths, so a bind
    // mount dockerd resolves in the sidecar finds what the model wrote.
    let mut shared_mounts = user_mounts;
    shared_mounts.extend([
        mount(WORKSPACE_VOLUME, WORKSPACE_DIR),
        mount(DOCKER_SOCK_VOLUME, DOCKER_SOCK_DIR),
    ]);
    let mut docker_mounts = shared_mounts.clone();
    docker_mounts.push(mount(DOCKER_DATA_VOLUME, "/var/lib/docker"));

    // Only a pod on a conversation's claim needs finding by conversation:
    // see `wait_for_conversation_pods_gone`.
    let labels = match docker.storage {
        PodStorage::Conversation(conversation_id) => Some(
            [(CONVERSATION_LABEL.to_string(), conversation_id.to_string())].into(),
        ),
        #[cfg(test)]
        PodStorage::Ephemeral => None,
    };

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(NAMESPACE.to_string()),
            labels,
            ..Default::default()
        },
        spec: Some(PodSpec {
            // A native sidecar (an init container with `restartPolicy:
            // Always`): the pod stays `Pending` until its startup probe
            // passes, so the sandbox never starts before dockerd answers,
            // and Kubernetes restarts it on its own after an OOM kill
            // without touching the sandbox container (SME-33's spike).
            init_containers: Some(vec![Container {
                name: "docker".to_string(),
                image: Some(default_docker_image()),
                // Delivered into the node like the sandbox image, see
                // scripts/build-sandbox-image.sh.
                image_pull_policy: Some("Never".to_string()),
                restart_policy: Some("Always".to_string()),
                security_context: Some(SecurityContext {
                    privileged: Some(true),
                    ..Default::default()
                }),
                command: Some(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    START_DOCKERD_SCRIPT.to_string(),
                    "start-dockerd".to_string(),
                ]),
                args: Some(dockerd_args()),
                env: Some(vec![EnvVar {
                    name: "WORKSPACE_OWNER".to_string(),
                    value: Some(SANDBOX_OWNER.to_string()),
                    ..Default::default()
                }]),
                startup_probe: Some(Probe {
                    exec: Some(ExecAction {
                        command: Some(vec![
                            "docker".to_string(),
                            format!("--host={DOCKER_HOST}"),
                            "info".to_string(),
                        ]),
                    }),
                    period_seconds: Some(DOCKER_STARTUP_PROBE_PERIOD_SECS),
                    failure_threshold: Some(DOCKER_STARTUP_PROBE_CHECKS),
                    ..Default::default()
                }),
                resources: Some(memory_only(&docker.memory)),
                volume_mounts: Some(docker_mounts),
                ..Default::default()
            }]),
            containers: vec![Container {
                name: "sandbox".to_string(),
                image: Some(default_sandbox_image()),
                // `Never`, not the `:latest`-tag default of `Always`: this
                // image is delivered straight into the node's local image
                // store (`ctr images import`, see
                // scripts/build-sandbox-image.sh) with no registry
                // involved at all — a real pull would just fail (there's
                // no such image on any real registry to pull), which is
                // exactly what happened the first time this was left unset.
                image_pull_policy: Some("Never".to_string()),
                // No `command` override — the image's own `ENTRYPOINT` is
                // the sandbox agent, so it's already running (and keeping
                // the pod alive) the moment the container starts. See
                // SME-17.
                resources: Some(memory_only(memory)),
                volume_mounts: Some(shared_mounts),
                ..Default::default()
            }],
            volumes: Some(pod_volumes),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    }
}

/// A container's resources (SME-77): a memory limit, an explicit zero
/// memory request and nothing for CPU. Without the request Kubernetes
/// copies the limit into it, so every idle pod reserved its whole limit
/// on the node; with no CPU limit or request, pods share the node's CPU.
fn memory_only(memory: &str) -> ResourceRequirements {
    let quantity = |q: &str| std::collections::BTreeMap::from([("memory".to_string(), Quantity(q.to_string()))]);
    ResourceRequirements {
        limits: Some(quantity(memory)),
        requests: Some(quantity("0")),
        ..Default::default()
    }
}

fn mount(volume: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: volume.to_string(),
        mount_path: path.to_string(),
        ..Default::default()
    }
}

fn empty_dir_volume(name: &str) -> Volume {
    Volume {
        name: name.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    }
}

/// /workspace: the conversation's own claim, so a new pod finds it as the
/// last one left it (SME-32).
fn workspace_volume(storage: &PodStorage) -> Volume {
    match storage {
        PodStorage::Conversation(conversation_id) => Volume {
            name: WORKSPACE_VOLUME.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: workspace_pvc_name(*conversation_id),
                ..Default::default()
            }),
            ..Default::default()
        },
        #[cfg(test)]
        PodStorage::Ephemeral => empty_dir_volume(WORKSPACE_VOLUME),
    }
}

fn docker_data_volume(storage: &PodStorage) -> Volume {
    match storage {
        PodStorage::Conversation(conversation_id) => Volume {
            name: DOCKER_DATA_VOLUME.to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: docker_pvc_name(*conversation_id),
                ..Default::default()
            }),
            ..Default::default()
        },
        #[cfg(test)]
        PodStorage::Ephemeral => empty_dir_volume(DOCKER_DATA_VOLUME),
    }
}

/// dockerd's arguments after `START_DOCKERD_SCRIPT` — a unix socket only,
/// never TCP: the pod shares one network namespace, so a TCP listener
/// would be reachable through `open_pod_target` and a preview link.
fn dockerd_args() -> Vec<String> {
    vec![
        format!("--host={DOCKER_HOST}"),
        format!("--group={DOCKER_GID}"),
        format!("--bip={DOCKER_BRIDGE_IP}"),
        "--default-address-pool".to_string(),
        format!("base={DOCKER_NETWORK_POOL},size=24"),
    ]
}

// Only called by the "no-agent pod" real-cluster test below — `create`
// itself now goes straight to `create_with_running_timeout`.
#[cfg_attr(not(test), allow(dead_code))]
async fn wait_for_running(pods: &Api<Pod>, name: &str) -> Result<(), SandboxError> {
    wait_for_running_with_timeout(pods, name, running_wait_timeout()).await
}

async fn wait_for_running_with_timeout(
    pods: &Api<Pod>,
    name: &str,
    timeout: Duration,
) -> Result<(), SandboxError> {
    let mut last_detail = None;
    let waited = tokio::time::timeout(timeout, async {
        loop {
            let pod = pods.get(name).await?;
            if let Some(reason) = pod_startup_failure(&pod) {
                return Err(SandboxError::StartFailed(reason));
            }
            last_detail = pod_pending_detail(&pod);
            if pod.status.and_then(|s| s.phase).as_deref() == Some("Running") {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    waited.map_err(|_| SandboxError::Timeout(last_detail))?
}

async fn drain_cleanup_queue(client: kube::Client, mut rx: mpsc::UnboundedReceiver<String>) {
    let pods = pods_api(&client);
    while let Some(name) = rx.recv().await {
        match tokio::time::timeout(
            Duration::from_secs(30),
            pods.delete(&name, &pod_delete_params()),
        )
        .await
        {
            Ok(Ok(_)) => tracing::info!(pod = %name, "cleaned up dropped sandbox"),
            Ok(Err(e)) => {
                tracing::error!(pod = %name, error = %e, "failed to clean up dropped sandbox")
            }
            Err(_) => tracing::error!(pod = %name, "timed out cleaning up dropped sandbox"),
        }
    }
}

// --- Process-global manager singleton (mirrors db::init()/db::get()) ---

static MANAGER: OnceLock<SandboxManager> = OnceLock::new();

pub async fn init() -> &'static SandboxManager {
    // `main()` already calls this before `init()` — but `init()` is also
    // called directly by `browser_tests.rs`'s in-process test harness,
    // which never runs through `main()` at all. Idempotent (`let _ = ...`
    // ignores the "already installed" error), so calling it again here is
    // safe regardless of caller. See `main()`'s own call site for the full
    // "why ring, not aws-lc-rs" explanation.
    let _ = rustls::crypto::ring::default_provider().install_default();

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
    MANAGER
        .get()
        .expect("sandbox not initialized; call sandbox::init() first")
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

/// How long a request to the agent waits for its reply.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte stream to a pod's agent.
trait AgentIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AgentIo for T {}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// How smelt reaches a pod's agent, and what it asks the cluster about the
/// pod along the way. `ClusterDialer` is the real one; tests put a fake
/// agent on a loopback port behind `dialer_for` instead.
trait AgentDialer: Send + Sync {
    fn dial(&self, pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>>;
    /// Whether the pod is `Running`: a first connection waits for that.
    fn is_running(&self, pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>>;
    /// `Some(reason)` once the cluster says the pod is dead, `None` while it
    /// may still be alive. See `pod_death_reason`.
    fn death_reason(&self, pod_id: i64) -> BoxFuture<'_, Option<Option<String>>>;
}

/// A port-forward to the agent's port in the pod.
struct ClusterDialer(kube::Client);

impl AgentDialer for ClusterDialer {
    fn dial(&self, pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>> {
        Box::pin(async move {
            let mut forward = pods_api(&self.0).portforward(&pod_name(pod_id), &[AGENT_PORT]).await?;
            let stream = forward.take_stream(AGENT_PORT).ok_or_else(|| {
                SandboxError::Io(std::io::Error::other("the port-forward has no stream for the agent's port"))
            })?;
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
fn dialer_for(pod_id: i64) -> Arc<dyn AgentDialer> {
    #[cfg(test)]
    if let Some(dialer) = test_dialers().lock().unwrap_or_else(|e| e.into_inner()).get(&pod_id) {
        return dialer.clone();
    }
    Arc::new(ClusterDialer(get().client.clone()))
}

/// Fake agents, by the pod id each test owns. Keyed by pod so tests running
/// in parallel never see each other's.
#[cfg(test)]
fn test_dialers() -> &'static StdMutex<HashMap<i64, Arc<dyn AgentDialer>>> {
    static DIALERS: LazyLock<StdMutex<HashMap<i64, Arc<dyn AgentDialer>>>> = LazyLock::new(Default::default);
    &DIALERS
}

/// One pod's agent connection. The WebSocket itself belongs to two tasks
/// `connect` starts: one writes `outgoing`, the other reads the agent's
/// messages (`handle_agent_message`).
struct TerminalConnection {
    outgoing: mpsc::UnboundedSender<String>,
    /// Requests waiting for their reply, by request id. `None` once the
    /// connection has ended: every waiter was told, and nothing new waits.
    pending: StdMutex<Option<HashMap<u64, tokio::sync::oneshot::Sender<Reply>>>>,
    /// The reader and writer tasks, stopped by `close`.
    tasks: StdMutex<Vec<tokio::task::AbortHandle>>,
    /// Request ids are per connection and never reused, so a late reply
    /// can't answer a newer request.
    next_request_id: std::sync::atomic::AtomicU64,
    /// Resolved once, when the connection is first established (see
    /// `connect`) — lets `handle_agent_message` publish a
    /// `SandboxCommandUpdate` for every output line and completion without
    /// a per-line DB round trip. See
    /// SME-10.
    conversation_id: i64,
    /// What the agent said it speaks, in its hello.
    agent_version: ProtocolVersion,
}

impl TerminalConnection {
    /// Sends a message that gets no reply (`command`, `signal`).
    fn send(&self, message: &ClientMessage) -> Result<(), AgentRequestError> {
        let text = serde_json::to_string(message).map_err(|e| AgentRequestError::Unencodable(e.to_string()))?;
        self.outgoing.send(text).map_err(|_| AgentRequestError::Disconnected)
    }

    /// Sends the request `build` makes with a fresh request id, and waits
    /// for its reply. The agent's own refusal (`Reply::Error`) comes back as
    /// `Rejected`.
    async fn request(&self, build: impl FnOnce(u64) -> ClientMessage) -> Result<Reply, AgentRequestError> {
        self.request_within(ACK_TIMEOUT, build).await
    }

    async fn request_within(
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

    fn forget(&self, request_id: u64) {
        if let Some(pending) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            pending.remove(&request_id);
        }
    }

    /// Hands `reply` to the request waiting for it. A reply nobody waits for
    /// (its request timed out) is dropped.
    fn resolve(&self, request_id: u64, reply: Reply) {
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
    fn fail_pending(&self) {
        // Dropping the senders wakes their receivers.
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Fails every waiting request and stops both tasks, which closes the
    /// socket.
    fn close(&self) {
        self.fail_pending();
        for task in self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            task.abort();
        }
    }

    /// Whether the agent has what minor `minor` of this major added. A
    /// feature added in a later minor checks this first, so an older pod
    /// fails that one tool and nothing else.
    #[cfg_attr(not(test), allow(dead_code))] // until a feature needs a minor above 0
    fn require_minor(&self, minor: u32) -> Result<(), TerminalError> {
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
struct Registry {
    connections: HashMap<i64, Arc<TerminalConnection>>,
    /// Pod ids never come back, so this is never cleared.
    torn_down: std::collections::HashSet<i64>,
    /// Pods whose agent speaks another major (or none): `found` from its
    /// hello. The image can't change under a pod, so it doesn't expire.
    outdated: HashMap<i64, Option<ProtocolVersion>>,
}

static REGISTRY: LazyLock<StdMutex<Registry>> = LazyLock::new(Default::default);

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

fn registry_get(pod_id: i64) -> Option<Arc<TerminalConnection>> {
    registry().connections.get(&pod_id).cloned()
}

fn registry_contains(pod_id: i64) -> bool {
    registry().connections.contains_key(&pod_id)
}

/// Why `register` refused a connection.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
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
fn register(pod_id: i64, conn: Arc<TerminalConnection>) -> Result<(), Refused> {
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
fn deregister(pod_id: i64) {
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
fn forget_connection(pod_id: i64) {
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
fn deregister_if_current(pod_id: i64, conn: &Arc<TerminalConnection>) -> bool {
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
fn outdated(pod_id: i64) -> Option<Option<ProtocolVersion>> {
    registry().outdated.get(&pod_id).copied()
}

/// How an agent on `agent` compares with a smelt on `smelt` of the same
/// major.
fn classify_agent(agent: ProtocolVersion, smelt: ProtocolVersion) -> crate::api::pods::AgentStatus {
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
fn pod_connect_lock(pod_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<StdMutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(pod_id)
        .or_default()
        .clone()
}

// --- Pod ---

/// The decision behind `create_pod`'s one-pod-per-conversation guard,
/// pulled out as a pure function over an already-fetched live-pod list so
/// it's unit-testable without a database or cluster — see
/// SME-11's "One pod per conversation."
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
/// `check_pod_guard`/`resolve_pod_id`. See SME-12's "Testing": the outer
/// `Option` is "confirmed dead or not" (`None` means genuinely
/// inconclusive — `Running`/`Pending` — not "no reason"); the inner one is
/// "did Kubernetes give a specific reason for it."
fn decide_pod_death_reason(pod: Option<Pod>) -> Option<Option<String>> {
    let Some(pod) = pod else {
        return Some(None); // pod object gone entirely — confirmed dead, nothing left to inspect
    };
    let status = pod.status.as_ref();
    // The sandbox container's status: by name, or the only one (tests'
    // pods, and pods from before the Docker sidecar, have just it).
    let sandbox = status.and_then(|s| s.container_statuses.as_ref()).and_then(|statuses| {
        statuses
            .iter()
            .find(|cs| cs.name == "sandbox")
            .or_else(|| statuses.first())
    });
    let current_end = sandbox
        .and_then(|cs| cs.state.as_ref())
        .and_then(|s| s.terminated.as_ref());
    let phase = status.and_then(|s| s.phase.as_deref());
    // Dead when the pod has failed, or when the sandbox container has
    // ended while the Docker sidecar keeps the pod Running as dockerd
    // shuts down (SME-51 B9). Anything else is inconclusive.
    if phase != Some("Failed") && current_end.is_none() {
        return None;
    }

    let container_terminated_reason = current_end
        .or_else(|| {
            sandbox
                .and_then(|cs| cs.last_state.as_ref())
                .and_then(|s| s.terminated.as_ref())
        })
        .and_then(|t| t.reason.clone());

    let reason = container_terminated_reason.or_else(|| pod.status.and_then(|s| s.reason));
    Some(reason)
}

/// `decide_pod_death_reason`'s real-world entry point — just the
/// `pods.get_opt` fetch, handed straight to the pure decision function.
/// An API call that itself fails is treated the same as "inconclusive"
/// (`None`), never as confirmation either way — see SME-12's "How."
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
/// itself. See SME-11's "One pod per conversation."
async fn conversation_pod_id(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
    let existing = db::list_sandbox_pods(pool, conversation_id).await?;
    resolve_pod_id(&existing)
}

/// Refuses if this conversation already has a live pod (see
/// `check_pod_guard`) — the model must `terminate_pod` before it can get
/// another. Rolls the DB row back (soft-terminates it) if the underlying
/// k8s create fails, so a failed create never leaves a pod_id `list_pods`
/// would show as live but that doesn't actually exist.
/// `limits` override the deployment's defaults for just this one pod —
/// see `PodLimitOverrides`.
pub async fn create_pod(
    pool: &PgPool,
    conversation_id: i64,
    limits: PodLimitOverrides,
) -> Result<i64, SandboxError> {
    let pool = pool.clone();
    run_pod_start(conversation_id, async move { create_pod_now(&pool, conversation_id, limits).await }).await
}

/// The conversation's running pod, starting one if it has none. Waits for
/// a pod that's still starting rather than taking its record early
/// (SME-51 B7).
pub async fn start_or_get_pod(pool: &PgPool, conversation_id: i64) -> Result<i64, SandboxError> {
    let pool = pool.clone();
    run_pod_start(conversation_id, async move {
        if let Ok(pod_id) = live_pod_id(&pool, conversation_id).await {
            return Ok(pod_id);
        }
        create_pod_now(&pool, conversation_id, PodLimitOverrides::default()).await
    })
    .await
}

/// Runs `start` in a task of its own, holding the conversation's pod-start
/// lock (SME-51 B7). Its own task: a Stop drops the turn that asked for the
/// pod, and a start cut off partway would leave a record with no pod, or a
/// pod never given its git setup. The lock makes a second start wait for
/// the first.
async fn run_pod_start<F>(conversation_id: i64, start: F) -> Result<i64, SandboxError>
where
    F: std::future::Future<Output = Result<i64, SandboxError>> + Send + 'static,
{
    let lock = pod_start_lock(conversation_id);
    tokio::spawn(async move {
        let _starting = lock.lock().await;
        start.await
    })
    .await
    .unwrap_or_else(|e| Err(SandboxError::StartFailed(format!("starting the sandbox failed: {e}"))))
}

/// After a failed pod start: if the conversation was deleted meanwhile,
/// removes what the start may have re-created after the delete's teardown
/// ran, its claims (SME-51 code review 1). `label_id` is the conversation
/// id its pods and claims are labelled with (the same, outside tests).
async fn clean_up_after_failed_start(pool: &PgPool, client: &kube::Client, conversation_id: i64, label_id: i64) {
    if !db::conversation_exists(pool, conversation_id).await.unwrap_or(true) {
        teardown_conversation_with(client, label_id, &[]).await;
    }
}

/// `create_pod_attempt`, cleaning up after a failure for a conversation
/// deleted while it ran.
async fn create_pod_now(
    pool: &PgPool,
    conversation_id: i64,
    limits: PodLimitOverrides,
) -> Result<i64, SandboxError> {
    let result = create_pod_attempt(pool, conversation_id, limits).await;
    // Only a deleted conversation needs the cluster touched; a refused
    // start for a live one (a pod already exists, say) doesn't.
    if result.is_err() && !db::conversation_exists(pool, conversation_id).await.unwrap_or(true) {
        clean_up_after_failed_start(pool, &get().client, conversation_id, conversation_id).await;
    }
    result
}

async fn create_pod_attempt(
    pool: &PgPool,
    conversation_id: i64,
    limits: PodLimitOverrides,
) -> Result<i64, SandboxError> {
    let existing = db::list_sandbox_pods(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    check_pod_guard(&existing)?;

    let (memory, docker) = limits.resolve(conversation_id);

    let manager = get();
    // The database's own one-live-pod rule backs up the check above.
    let row = db::create_sandbox_pod(pool, conversation_id).await.map_err(|e| {
        if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
            SandboxError::PodAlreadyExists
        } else {
            SandboxError::Db(e)
        }
    })?;
    let volumes = db::list_sandbox_volumes(pool)
        .await
        .map_err(SandboxError::Db)?;
    // Reuses SandboxManager::create's existing get-or-create-on-Running
    // logic rather than duplicating it — the returned Sandbox is only a
    // handle for exec/Drop-cleanup purposes, neither of which apply here
    // (this pod persists independently of any in-process value), so it's
    // disarmed immediately, the same `mem::forget` pattern
    // `SandboxManager::delete` itself already uses for the same reason.
    // A pod terminate_pod just stopped can still hold the Docker claim.
    if let Err(e) =
        wait_for_conversation_pods_gone(&manager.client, conversation_id, running_wait_timeout())
            .await
    {
        let _ = db::terminate_sandbox_pod(pool, row.id).await;
        return Err(e);
    }
    if let Err(e) = ensure_conversation_pvcs(&manager.client, conversation_id).await {
        let _ = db::terminate_sandbox_pod(pool, row.id).await;
        return Err(e);
    }
    match manager
        .create_with_docker(&row.id.to_string(), &memory, &docker, &volumes)
        .await
    {
        Ok(sandbox) => {
            // Keys and the commit identity, before anything can run a git
            // command (SME-32). A pod without them would fail its first
            // push with a confusing ssh error, so a failure here is the
            // pod's failure.
            if let Err(e) = crate::git::install_into_new_pod(pool, row.id).await {
                let _ = manager.delete(sandbox).await;
                let _ = db::terminate_sandbox_pod(pool, row.id).await;
                return Err(SandboxError::GitSetup(e));
            }
            std::mem::forget(sandbox);
            // The conversation may have been deleted while this pod was
            // starting; its teardown had nothing to find yet (SME-51 B5).
            if !db::conversation_exists(pool, conversation_id).await.unwrap_or(true) {
                teardown_conversation_with(&manager.client, conversation_id, &[]).await;
                return Err(SandboxError::StartFailed(
                    "the conversation was deleted while its sandbox was starting".to_string(),
                ));
            }
            events::publish(
                conversation_id,
                events::ConversationEvent::SandboxPodUpdate {
                    pod_id: row.id,
                    status: "Running".to_string(),
                    terminated: false,
                },
            );
            events::publish_app(events::AppEvent::PodsChanged);
            Ok(row.id)
        }
        Err(e) => {
            let _ = db::terminate_sandbox_pod(pool, row.id).await;
            Err(e)
        }
    }
}

/// Each conversation's "a pod is being started" lock (SME-51 B7).
fn pod_start_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<StdMutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(conversation_id)
        .or_default()
        .clone()
}

/// Refuses the ports in a pod no route may reach: the sandbox agent's
/// (`AGENT_PORT`), whose WebSocket runs commands with no login of its own,
/// and its relay (`RELAY_PORT`), which reaches every container. A preview
/// or a page in the model's browser reaching either would let any website
/// into the sandbox (found in SME-42's code review).
pub fn check_reachable_port(port: u16) -> Result<(), TerminalError> {
    if port == AGENT_PORT || port == RELAY_PORT {
        return Err(TerminalError::AgentPort(port));
    }
    Ok(())
}

/// `create_pod`'s per-pod overrides of the deployment's memory limits:
/// the sandbox container's (`SANDBOX_MEMORY_LIMIT`, see SME-12's "Per-pod
/// limit overrides") and the Docker sidecar's
/// (`SANDBOX_DOCKER_MEMORY_LIMIT`, SME-33). Plain Kubernetes quantity
/// strings, validated by Kubernetes itself against the namespace's
/// `LimitRange`. There's no CPU limit to override (SME-77).
#[derive(Debug, Clone, Default)]
pub struct PodLimitOverrides {
    pub memory: Option<String>,
    pub docker_memory: Option<String>,
}

impl PodLimitOverrides {
    /// The sandbox container's memory limit, and the Docker sidecar on the
    /// conversation's own claim.
    fn resolve(self, conversation_id: i64) -> (String, DockerSidecar) {
        let docker = DockerSidecar {
            memory: self.docker_memory.unwrap_or_else(default_docker_memory_limit),
            storage: PodStorage::Conversation(conversation_id),
        };
        (self.memory.unwrap_or_else(default_memory_limit), docker)
    }
}

/// Where in a conversation's pod a connection goes (SME-33): its own
/// `localhost`, or a Docker container's address in the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodHost {
    Localhost,
    Container(std::net::Ipv4Addr),
}

/// A byte stream to a port inside a sandbox pod, from `open_pod_target`.
pub trait PodIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> PodIo for T {}

/// Opens a TCP connection to `host:port` inside `conversation_id`'s own live
/// pod, through a Kubernetes port-forward — the pod is resolved here from
/// the conversation, never named by the caller, so nothing can reach
/// another conversation's pod (SME-42). The connection is made inside the
/// pod's network namespace, so a server bound only to `127.0.0.1` there
/// is reachable. One port-forward per connection.
///
/// Nothing listening on `port` isn't an error here: the stream simply
/// ends without a byte. After the pod side closes, the end of the stream
/// arrives about a second late (seen on k3s), so a caller must never wait
/// for it to know a response is complete.
///
/// `host` is the pod's own `localhost`, or a Docker container's address
/// there (SME-33), reached through the agent's relay.
pub async fn open_pod_target(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
) -> Result<Box<dyn PodIo>, TerminalError> {
    open_pod_port_with(pool, conversation_id, host, port, || get().client.clone()).await
}

/// `open_pod_target` with the Kubernetes client supplied — a test's own, so
/// it never has to set the process-global manager (see
/// `test_open_pod_port_reaches_the_conversations_own_pod`). The client is
/// only asked for once there's a pod, so a conversation without one never
/// needs a cluster at all.
async fn open_pod_port_with(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
    client: impl FnOnce() -> kube::Client,
) -> Result<Box<dyn PodIo>, TerminalError> {
    if host == PodHost::Localhost {
        check_reachable_port(port)?;
    }
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    dial_pod(&client(), &pod_name(pod_id), host, port).await
}

/// Opens a connection to `host:port` in the pod named `pod`: a
/// port-forward for `localhost`, or one to the agent's relay for a
/// container address.
async fn dial_pod(
    client: &kube::Client,
    pod: &str,
    host: PodHost,
    port: u16,
) -> Result<Box<dyn PodIo>, TerminalError> {
    let forward_to = match host {
        PodHost::Localhost => {
            check_reachable_port(port)?;
            port
        }
        PodHost::Container(_) => RELAY_PORT,
    };
    let mut forwarder = pods_api(client)
        .portforward(pod, &[forward_to])
        .await
        .map_err(|e| TerminalError::Sandbox(SandboxError::Kube(e)))?;
    let mut stream = forwarder
        .take_stream(forward_to)
        .expect("stream requested for the forwarded port");
    if let PodHost::Container(ip) = host {
        // The relay's one line, then its answer once it has connected
        // (`RELAY_CONNECTED` in src/bin/sandbox_agent.rs). A target it
        // refuses, or can't reach, gets a stream that's already ended,
        // which reads the same as a port nothing listens on.
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(format!("{ip}:{port}\n").as_bytes())
            .await
            .map_err(|e| TerminalError::Sandbox(SandboxError::Io(e)))?;
        let mut answer = [0u8; 3];
        let connected = tokio::time::timeout(RELAY_CONNECT_WAIT, stream.read_exact(&mut answer))
            .await
            .is_ok_and(|read| read.is_ok() && &answer == b"ok\n");
        if !connected {
            return Ok(Box::new(tokio::io::join(tokio::io::empty(), tokio::io::sink())));
        }
    }
    Ok(Box::new(stream))
}

/// Whether something is listening on `port` in `conversation_id`'s pod —
/// the sharing a preview's check, so the model hears at once if it gave
/// the wrong port or the server isn't up yet. A port-forward to a port
/// nothing listens on ends straight away without a byte (about 50ms on
/// k3s); a listening server keeps the connection open waiting for a
/// request. `LISTEN_PROBE` is well past the first and far below a request
/// timeout.
pub async fn pod_port_is_listening(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
) -> Result<bool, TerminalError> {
    pod_port_is_listening_with(pool, conversation_id, host, port, || get().client.clone()).await
}

const LISTEN_PROBE: Duration = Duration::from_millis(500);

/// How long opening the port-forward for the probe may take (SME-51 B8).
const LISTEN_OPEN_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(10) };

async fn pod_port_is_listening_with(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
    client: impl FnOnce() -> kube::Client,
) -> Result<bool, TerminalError> {
    // A port-forward that doesn't open in time says as much as one that's
    // refused: nothing is serving there yet (SME-51 B8).
    let Ok(opened) =
        tokio::time::timeout(LISTEN_OPEN_TIMEOUT, open_pod_port_with(pool, conversation_id, host, port, client)).await
    else {
        return Ok(false);
    };
    Ok(stream_is_listening(opened?).await)
}

/// `pod_port_is_listening`'s judgement of a freshly opened stream.
async fn stream_is_listening(mut stream: Box<dyn PodIo>) -> bool {
    let mut first = [0u8; 1];
    match tokio::time::timeout(LISTEN_PROBE, stream.read(&mut first)).await {
        // Still open: something accepted the connection and is waiting.
        Err(_) => true,
        // A server that speaks first (SSH, a database) is listening too.
        Ok(Ok(n)) => n > 0,
        Ok(Err(_)) => false,
    }
}

/// The conversation's live pod's id, for callers outside this module —
/// `NoPod` when it has none.
pub async fn live_pod_id(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
    conversation_pod_id(pool, conversation_id).await
}

/// Takes `conversation_id`, resolved to "the conversation's pod" via
/// `conversation_pod_id` — no longer idempotent on repeat the way a
/// `pod_id`-addressed version was: once the one live pod is terminated,
/// there's no longer a live pod for this conversation to resolve, so a
/// second call fails clearly with `NoPod` ("call create_pod first") rather
/// than silently succeeding again. See SME-11's "How." Refuses if the
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
/// `connect_with_retry`'s exhausted-retries fallback (unguarded —
/// nothing to check, it's already given up reaching this pod). Deregisters
/// *before* touching the k8s API, not after — see SME-12's "How" on why
/// that ordering is what lets a deliberate teardown always win the race
/// against the connection's own reader task noticing the drop.
async fn force_terminate_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<db::SandboxPod>, SandboxError> {
    terminate_pod_with(pool, pod_id, async {
        let pods = pods_api(&get().client);
        let name = pod_name(pod_id);
        if pods.get_opt(&name).await?.is_some() {
            pods.delete(&name, &pod_delete_params()).await?;
        }
        Ok(())
    })
    .await
}

/// `force_terminate_pod` with the cluster's delete passed in, so a test can
/// make it fail.
async fn terminate_pod_with(
    pool: &PgPool,
    pod_id: i64,
    delete: impl std::future::Future<Output = Result<(), SandboxError>>,
) -> Result<Option<db::SandboxPod>, SandboxError> {
    deregister(pod_id);
    // The pod stays running and its row live if either step fails, so it
    // must stay reachable too.
    let closed = async {
        delete.await?;
        db::terminate_sandbox_pod(pool, pod_id).await.map_err(SandboxError::Db)
    }
    .await;
    if closed.is_err() {
        registry().torn_down.remove(&pod_id);
    }
    let row = closed?;
    if let Some(row) = &row {
        events::publish(
            row.conversation_id,
            events::ConversationEvent::SandboxPodUpdate {
                pod_id,
                status: "terminated".to_string(),
                terminated: true,
            },
        );
        events::publish_app(events::AppEvent::PodsChanged);
    }
    Ok(row)
}

/// What the pods view shows about a pod from Kubernetes itself: its phase
/// and each of its containers' configured limits (as Kubernetes quantity
/// strings): the sandbox container's and, since SME-33, the Docker
/// sidecar's. The view adds them up, as it does their usage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PodDetails {
    pub phase: Option<String>,
    pub memory_limits: Vec<String>,
    pub cpu_limits: Vec<String>,
}

/// Every running container's memory and CPU limits in `pod`: its
/// containers and its native sidecars (init containers that keep running).
fn pod_container_limits(pod: &Pod) -> (Vec<String>, Vec<String>) {
    let Some(spec) = pod.spec.as_ref() else {
        return (Vec::new(), Vec::new());
    };
    let sidecars = spec
        .init_containers
        .iter()
        .flatten()
        .filter(|c| c.restart_policy.as_deref() == Some("Always"));
    let running: Vec<&Container> = spec.containers.iter().chain(sidecars).collect();
    let limits = |name: &str| -> Vec<String> {
        running
            .iter()
            .filter_map(|c| c.resources.as_ref()?.limits.as_ref()?.get(name))
            .map(|q| q.0.clone())
            .collect()
    };
    (limits("memory"), limits("cpu"))
}

/// `pod_id`'s phase and limits, or `None` if Kubernetes has no such pod.
pub async fn pod_details(pod_id: i64) -> Result<Option<PodDetails>, SandboxError> {
    let pods = pods_api(&get().client);
    let Some(pod) = pods.get_opt(&pod_name(pod_id)).await? else {
        return Ok(None);
    };
    let (memory_limits, cpu_limits) = pod_container_limits(&pod);
    Ok(Some(PodDetails {
        phase: pod.status.as_ref().and_then(|s| s.phase.clone()),
        memory_limits,
        cpu_limits,
    }))
}

/// The raw `PodMetricsList` for smelt's namespace, from the cluster's
/// metrics API (metrics-server). Needs `get`/`list` on `pods` in the
/// `metrics.k8s.io` group (`k8s/smelt-park-rbac.yaml`); without it this is
/// a 403 error, which callers treat as "usage unavailable".
pub async fn pod_metrics_list() -> Result<serde_json::Value, SandboxError> {
    let request = http::Request::get(format!(
        "/apis/metrics.k8s.io/v1beta1/namespaces/{NAMESPACE}/pods"
    ))
    .body(Vec::new())
    .expect("a static, well-formed request");
    Ok(get().client.request::<serde_json::Value>(request).await?)
}

/// The Kubernetes pod name for `pod_id`, for matching metrics to rows.
pub fn kubernetes_pod_name(pod_id: i64) -> String {
    pod_name(pod_id)
}

pub async fn list_pods(pool: &PgPool, conversation_id: i64) -> Result<Vec<PodInfo>, SandboxError> {
    let manager = get();
    let pods = pods_api(&manager.client);
    let rows = db::list_sandbox_pods(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let name = pod_name(row.id);
        let status = pods
            .get_opt(&name)
            .await?
            .and_then(|p| p.status)
            .and_then(|s| s.phase)
            .unwrap_or_else(|| "Unknown".to_string());
        result.push(PodInfo {
            pod_id: row.id,
            status,
            agent: agent_status(row.id),
        });
    }
    Ok(result)
}

// --- Generic volumes ---

/// `SANDBOX_VOLUME_STORAGE_SIZE`, default `"10Gi"` — same pattern as
/// `default_memory_limit`. Every generic volume's PVC requests this much
/// capacity; not currently configurable per volume (nothing in
/// SME-17 calls for that), just
/// a documented fixed default.
fn default_volume_storage_size() -> String {
    std::env::var("SANDBOX_VOLUME_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "10Gi".to_string())
}

fn build_volume_pvc_spec(volume_id: i64) -> PersistentVolumeClaim {
    let mut requests = std::collections::BTreeMap::new();
    requests.insert(
        "storage".to_string(),
        Quantity(default_volume_storage_size()),
    );

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(sandbox_volume_pvc_name(volume_id)),
            namespace: Some(NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Creates a new generic volume: resolves a leading `~` in `mount_path`
/// (see `resolve_mount_path`), inserts the `sandbox_volumes` row, then
/// creates its backing PVC — rolled back (DB row deleted) if the PVC
/// create fails, so a failed create never leaves an orphaned row
/// `list_sandbox_volumes` would show as usable but that doesn't actually
/// have a PVC behind it. Same rollback shape `create_pod` already uses for
/// its own DB-row-then-k8s-object ordering.
pub async fn create_volume(
    pool: &PgPool,
    name: &str,
    mount_path: &str,
) -> Result<i64, SandboxError> {
    let resolved_path = resolve_mount_path(mount_path);
    validate_mount_path(&resolved_path).map_err(SandboxError::InvalidMountPath)?;
    let row = db::create_sandbox_volume(pool, name, &resolved_path)
        .await
        .map_err(SandboxError::Db)?;

    let manager = get();
    let pvcs = pvc_api(&manager.client);
    if let Err(e) = pvcs
        .create(&PostParams::default(), &build_volume_pvc_spec(row.id))
        .await
    {
        let _ = db::delete_sandbox_volume(pool, row.id).await;
        return Err(e.into());
    }
    Ok(row.id)
}

/// Deletes a generic volume's PVC, then its `sandbox_volumes` row. A
/// missing PVC (already gone) is not an error — same "delete if it's still
/// there" tolerance `force_terminate_pod` already has for its own pod.
pub async fn delete_volume(pool: &PgPool, id: i64) -> Result<(), SandboxError> {
    let manager = get();
    let pvcs = pvc_api(&manager.client);
    let pvc_name = sandbox_volume_pvc_name(id);
    if pvcs.get_opt(&pvc_name).await?.is_some() {
        pvcs.delete(&pvc_name, &DeleteParams::default()).await?;
    }
    db::delete_sandbox_volume(pool, id)
        .await
        .map_err(SandboxError::Db)?;
    Ok(())
}

// --- Terminal ---

/// Takes `conversation_id`, resolved to "the conversation's pod" via
/// `conversation_pod_id` — errors `NoPod` if none exists yet (create_pod
/// first), same requirement as before, just checked against the DB instead
/// of trusting a caller-supplied `pod_id`. Every call creates a genuinely
/// new terminal in that pod — no idempotency to preserve with N terminals
/// per pod. Establishes the pod's agent connection if it isn't already
/// live in this process's registry — the agent itself is already running
/// by the time the pod is `Running` (it's the pod's own `ENTRYPOINT`, see
/// `ensure_pod_connection`), so this is just "connect," never "launch."
pub async fn create_terminal(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = ensure_pod_connection(pool, pod_id).await?;

    let row = db::create_sandbox_terminal(pool, pod_id).await?;
    let terminal_id = row.id;

    let created = conn
        .request(|request_id| ClientMessage::CreateTerminal { request_id, terminal_id: terminal_id.to_string() })
        .await;
    if let Err(e) = expect_reply(created, "create_terminal", |reply| {
        matches!(reply, Reply::TerminalCreated).then_some(())
    }) {
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

/// Idempotent on repeat (see SME-9's "How"). Refuses if a command is
/// still `running` in this terminal — the model must `send_signal`/wait
/// it out first. Otherwise asks the agent to `killpg` just this
/// terminal's shell — see SME-9's "Terminating a terminal without
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
    let terminated = conn
        .request(|request_id| ClientMessage::TerminateTerminal { request_id, terminal_id: terminal_id.to_string() })
        .await;
    expect_reply(terminated, "terminate_terminal", |reply| {
        matches!(reply, Reply::TerminalTerminated).then_some(())
    })?;

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
pub async fn list_terminals(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<TerminalInfo>, SandboxError> {
    let rows = db::list_sandbox_terminals_for_conversation(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    Ok(rows
        .into_iter()
        .map(|t| TerminalInfo {
            terminal_id: t.id,
            pod_id: t.pod_id,
            status: if registry_contains(t.pod_id) {
                "connected"
            } else {
                "disconnected"
            }
            .to_string(),
        })
        .collect())
}

/// Sends a `command` to the terminal's pod's agent — reconnecting first if the
/// registry has no live entry (a smelt restart, or the first send right
/// after `create_terminal`'s own connect). Never launches a fresh agent
/// itself (that's `create_terminal`'s job).
pub async fn send_command(
    pool: &PgPool,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> Result<(), TerminalError> {
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id)
        .await?
        .ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    Ok(conn.send(&ClientMessage::Command {
        terminal_id: terminal_id.to_string(),
        id: command_id.to_string(),
        command: command.to_string(),
    })?)
}

/// Sends a `signal` — same reconnect-first, never-launches behavior as
/// `send_command`.
pub async fn send_signal(
    pool: &PgPool,
    terminal_id: i64,
    command_id: &str,
    signal: &str,
) -> Result<(), TerminalError> {
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id)
        .await?
        .ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    Ok(conn.send(&ClientMessage::Signal {
        terminal_id: terminal_id.to_string(),
        id: command_id.to_string(),
        signal: signal.to_string(),
    })?)
}

/// Deletes every pod that exists for this conversation, unconditionally
/// (unlike `terminate_pod`, this is a hard teardown on conversation
/// deletion, not a guarded API the model calls) — see SME-9's
/// `chat.rs`/`main.rs` bullet. The DB rows themselves don't need clearing
/// here: `db::delete_conversation`'s `ON DELETE CASCADE` chain removes
/// `sandbox_pods`/`sandbox_terminals`/`terminal_commands` for real right
/// after this runs.
/// Whether sandbox pod `pod_id` still exists in the cluster (terminating
/// counts as existing).
#[cfg(all(test, feature = "browser-test"))]
pub(crate) async fn pod_exists(pod_id: i64) -> bool {
    matches!(
        pods_api(&get().client).get_opt(&pod_name(pod_id)).await,
        Ok(Some(_))
    )
}

pub async fn teardown_conversation(conversation_id: i64, pod_ids: &[i64]) {
    teardown_conversation_with(&get().client, conversation_id, pod_ids).await;
}

/// `teardown_conversation` on `client`. Pods are found by their
/// conversation label: a `create_pod` racing the conversation's deletion
/// makes a pod whose record the delete then cascades away (SME-51 B5).
/// `pod_ids`, the conversation's live pod records read before the delete,
/// name the rest (SME-88).
async fn teardown_conversation_with(client: &kube::Client, conversation_id: i64, pod_ids: &[i64]) {
    let pods = pods_api(client);
    // Its sandbox pods, and its language server pods (SME-35).
    for label in [CONVERSATION_LABEL, crate::lsp::pods::LSP_OF_LABEL] {
        let selector = ListParams::default().labels(&format!("{label}={conversation_id}"));
        delete_listed(&pods, &selector, conversation_id).await;
    }
    // And the pods its records name, labelled or not (one from before
    // SME-33 has no label; SME-88). A pod deleted above is already gone.
    for &pod_id in pod_ids {
        deregister(pod_id);
        match pods.delete(&pod_name(pod_id), &pod_delete_params()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => tracing::warn!(pod_id, error = %e, "failed to delete pod during conversation teardown"),
        }
    }
    // After the pods: Kubernetes holds a claim until no pod mounts it.
    delete_conversation_pvcs(client, conversation_id).await;
}

async fn delete_listed(pods: &Api<Pod>, selector: &ListParams, conversation_id: i64) {
    match pods.list(selector).await {
        Ok(list) => {
            for pod in list {
                if let Some(pod_id) = watched_pod_id(&pod) {
                    deregister(pod_id);
                }
                let Some(name) = pod.metadata.name else { continue };
                if let Err(e) = pods.delete(&name, &pod_delete_params()).await {
                    tracing::warn!(pod = %name, error = %e, "failed to delete pod during conversation teardown");
                }
            }
        }
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't list a deleted conversation's pods"),
    }
}

/// Returns the existing registry entry for `pod_id` if there is one;
/// otherwise tries to (re)connect to that pod's agent. This is what makes
/// a smelt restart transparently reconnect to a still-healthy agent, *and*
/// what detects a crashed agent (pod exists, `Running`, but nothing
/// answers) — see SME-9's "Agent crash recovery" and `connect_with_retry`.
async fn reconnect_if_needed(
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
const RECONNECT_ATTEMPTS: u32 = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Who is connecting, which decides what a failure means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectMode {
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
enum ConnectError {
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
fn connect_with_retry(
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

        let dialer = dialer_for(pod_id);
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
async fn ensure_pod_connection(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Arc<TerminalConnection>, TerminalError> {
    if let Some(conn) = registry_get(pod_id) {
        return Ok(conn);
    }
    connect_with_retry(pool.clone(), pod_id, ConnectMode::First).await
}

/// The value `pick` finds in a request's reply, or why there isn't one.
fn expect_reply<T>(
    result: Result<Reply, AgentRequestError>,
    action: &'static str,
    pick: impl FnOnce(Reply) -> Option<T>,
) -> Result<T, TerminalError> {
    pick(result?).ok_or(TerminalError::Agent(AgentRequestError::UnexpectedReply(action)))
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
    let reply = conn
        .request(|request_id| ClientMessage::ReadFile { request_id, path: path.to_string(), offset, limit })
        .await;
    expect_reply(reply, "read_file", |reply| match reply {
        Reply::FileRead(contents) => Some(contents),
        _ => None,
    })
}

/// Creates or overwrites `path` in this conversation's pod. `expected_hash`
/// is `None` only for a brand-new file (the read-before-write check has
/// nothing to have read yet) — see
/// SME-11's "Read-before-write discipline."
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
    let reply = conn
        .request(|request_id| ClientMessage::WriteFile {
            request_id,
            path: path.to_string(),
            content: content.to_string(),
            expected_hash,
        })
        .await;
    expect_reply(reply, "write_file", |reply| match reply {
        Reply::FileWritten { hash } => Some(hash),
        _ => None,
    })
}

/// Applies a targeted `old_string` → `new_string` replacement to `path` in
/// this conversation's pod. `expected_hash` is always required (unlike
/// `write_file`) — `edit_file` always needs a prior read to have produced
/// the `old_string` it's matching against. `expected_line`, if set, targets
/// one specific occurrence instead of requiring a file-wide unique match —
/// see SME-11's "What" on `edit_file`. Returns the new content's hash.
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
    let reply = conn
        .request(|request_id| ClientMessage::EditFile {
            request_id,
            path: path.to_string(),
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
            replace_all,
            expected_hash,
            expected_line,
        })
        .await;
    expect_reply(reply, "edit_file", |reply| match reply {
        Reply::FileEdited { hash } => Some(hash),
        _ => None,
    })
}

/// Lists `path` (one level, non-recursive) in this conversation's pod.
pub async fn list_directory(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
) -> Result<Vec<DirEntry>, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::ListDirectory { request_id, path: path.to_string() })
        .await;
    expect_reply(reply, "list_directory", |reply| match reply {
        Reply::DirectoryListed { entries } => Some(entries),
        _ => None,
    })
}

/// Finds files under `path` whose path relative to `path` matches
/// `pattern`, paginated by `offset`/`limit` — see
/// SME-19.
pub async fn glob(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    pattern: &str,
    offset: u32,
    limit: u32,
) -> Result<GlobResult, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::Glob {
            request_id,
            path: path.to_string(),
            pattern: pattern.to_string(),
            offset,
            limit,
        })
        .await;
    expect_reply(reply, "glob", |reply| match reply {
        Reply::GlobMatched(result) => Some(result),
        _ => None,
    })
}

/// Searches file contents under `path` for `pattern`, optionally narrowed
/// to files matching `glob` first, paginated by `offset`/`limit` — see
/// SME-19.
pub async fn grep(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
    pattern: &str,
    glob: Option<String>,
    case_insensitive: bool,
    offset: u32,
    limit: u32,
) -> Result<GrepResult, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let reply = conn
        .request(|request_id| ClientMessage::Grep {
            request_id,
            path: path.to_string(),
            pattern: pattern.to_string(),
            glob,
            case_insensitive,
            offset,
            limit,
        })
        .await;
    expect_reply(reply, "grep", |reply| match reply {
        Reply::GrepMatched(result) => Some(result),
        _ => None,
    })
}

/// The user stopping a pod from the pods view or the sandbox panel. Tears
/// it down whether or not terminals are open, the way a crash does:
/// running commands are marked lost and terminals closed. The model is
/// told in one notice, saved between turns so it can't split a tool call
/// from its result. Doesn't wake the model: it learns on its next turn.
pub async fn stop_pod_for_user(pool: &PgPool, pod_id: i64) -> Result<(), TerminalError> {
    let live = db::sandbox_pod_is_live(pool, pod_id).await?;
    if !live {
        return Err(TerminalError::NoPod);
    }
    let (conversation_id, _) = close_pod_terminals(pool, pod_id).await;
    force_terminate_pod(pool, pod_id).await?;
    if let Some(conversation_id) = conversation_id {
        let notice = format!(
            "The user stopped sandbox pod {pod_id}. Its terminals, and any files outside /workspace and mounted volumes, are gone. Create a new pod if you need one."
        );
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::api::chat::save_notice_between_turns(&pool, conversation_id, notice).await {
                tracing::warn!(conversation_id, pod_id, error = %e, "couldn't save the pod stop notice");
            }
        });
    }
    Ok(())
}

/// How old a live pod row must be before the watch's listing may close it
/// for having no pod in Kubernetes: a row is written just before its pod
/// is created, and creating one can take a while. Also keeps a smelt
/// instance from closing another instance's brand-new rows (the browser
/// test harness shares the dev database but not its namespace).
const RECONCILE_MIN_AGE_SECS: i64 = 300;

/// How long after a pod is deleted (or finishes) the watch waits before
/// closing its record, so crash detection gets there first for a pod smelt
/// was connected to: it closes the same records, but also tells the model.
#[cfg(not(test))]
const CLOSE_GRACE: Duration = Duration::from_secs(30);
#[cfg(test)]
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Keeps pod records in step with Kubernetes for as long as smelt runs:
/// subscribes to pod changes in smelt's namespace, reconciles once the
/// first full listing arrives (and again after any reconnect, which
/// re-lists), then closes a pod's record when the pod is deleted or reaches
/// a phase it can't recover from. Covers pods lost while smelt wasn't
/// connected to them: deleted outside smelt, lost in a cluster rebuild,
/// evicted, or dead while smelt was down. See `close_if_gone`.
///
/// Only a real server runs this (from `main`), never the browser test
/// harness: that shares the dev database but works in the test namespace,
/// where the dev instance's pods don't exist.
pub async fn watch_pods(pool: PgPool) {
    use futures_util::StreamExt;
    use kube::runtime::{WatchStreamExt, watcher};

    let events = watcher(pods_api(&get().client), watcher::Config::default()).default_backoff();
    futures_util::pin_mut!(events);
    // Pods listed since the last `Init`, until `InitDone` completes the set.
    let mut listed = std::collections::HashSet::new();
    // Each pod's Docker sidecar, see `note_docker_restarts`.
    let mut docker_restarts = HashMap::new();
    // Server pods whose stop was seen, see `lsp::pods::note_server_stop`.
    let mut stopped_servers = std::collections::HashSet::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(watcher::Event::Init) => listed.clear(),
            Ok(watcher::Event::InitApply(pod)) => {
                if let Some(restart) = note_docker_restarts(&mut docker_restarts, &pod, true) {
                    handle_docker_restart(&pool, restart).await;
                }
                crate::lsp::pods::note_server_stop(&mut stopped_servers, &pod, true);
                if let Some(pod_id) = watched_pod_id(&pod) {
                    if !pod_has_finished(&pod) {
                        listed.insert(pod_id);
                    }
                }
            }
            Ok(watcher::Event::InitDone) => {
                forget_unlisted_docker(&mut docker_restarts, &listed);
                match db::live_pods_older_than(&pool, RECONCILE_MIN_AGE_SECS).await {
                    Ok(rows) => {
                        for row in rows.into_iter().filter(|row| !listed.contains(&row.id)) {
                            close_if_gone(&pool, row.id).await;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "couldn't list pod records to reconcile"),
                }
            }
            Ok(watcher::Event::Apply(pod)) if pod_has_finished(&pod) => {
                if let Some(stopped) = crate::lsp::pods::note_server_stop(&mut stopped_servers, &pod, false) {
                    let pool = pool.clone();
                    tokio::spawn(async move {
                        crate::api::chat::deliver_notice(&pool, stopped.conversation_id, stopped.notice).await;
                    });
                }
                if let Some(pod_id) = watched_pod_id(&pod) {
                    close_after_grace(pool.clone(), pod_id);
                }
            }
            Ok(watcher::Event::Apply(pod)) => {
                if let Some(restart) = note_docker_restarts(&mut docker_restarts, &pod, false) {
                    handle_docker_restart(&pool, restart).await;
                }
            }
            Ok(watcher::Event::Delete(pod)) => {
                if let Some(pod_id) = watched_pod_id(&pod) {
                    docker_restarts.remove(&pod_id);
                    close_after_grace(pool.clone(), pod_id);
                }
            }
            // The watcher backs off and retries on its own, re-listing
            // (a fresh `Init`..`InitDone`) once it's back.
            Err(e) => tracing::warn!(error = %e, "pod watch error"),
        }
    }
}

/// The smelt pod id behind a watched pod (`sandbox-{id}`), or `None` for
/// any other pod in the namespace.
fn watched_pod_id(pod: &Pod) -> Option<i64> {
    pod.metadata.name.as_deref()?.strip_prefix("sandbox-")?.parse().ok()
}

/// Whether a pod has stopped for good: `Succeeded` or `Failed` (sandbox
/// pods never restart).
fn pod_has_finished(pod: &Pod) -> bool {
    matches!(
        pod.status.as_ref().and_then(|s| s.phase.as_deref()),
        Some("Succeeded" | "Failed")
    )
}

/// `close_if_gone` for `pod_id`, after `CLOSE_GRACE`.
fn close_after_grace(pool: PgPool, pod_id: i64) {
    tokio::spawn(async move {
        tokio::time::sleep(CLOSE_GRACE).await;
        close_if_gone(&pool, pod_id).await;
    });
}

/// Closes `pod_id`'s record if it's still live, smelt has no connection to
/// it (a connected pod's end is crash detection's to report, notice
/// included), and Kubernetes really has no running pod for it. Running
/// commands are marked lost and terminals closed, like a crash, but
/// quietly: no notice to the model (saving one would move the conversation
/// to the top of the sidebar, and a cluster rebuild can leave dozens); it
/// finds out if it tries the pod again.
async fn close_if_gone(pool: &PgPool, pod_id: i64) {
    match db::sandbox_pod_is_live(pool, pod_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't check a pod record");
            return;
        }
    }
    if registry_get(pod_id).is_some() {
        return;
    }
    match pods_api(&get().client).get_opt(&pod_name(pod_id)).await {
        Ok(Some(pod)) if !pod_has_finished(&pod) => return,
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't check a pod in Kubernetes");
            return;
        }
    }
    close_pod_terminals(pool, pod_id).await;
    // Deletes a finished pod's object if it's still there, marks the row
    // terminated, and tells the UI (the sandbox panel, the sidebar, /pods).
    match force_terminate_pod(pool, pod_id).await {
        Ok(_) => tracing::info!(pod_id, "closed the record of a pod that's gone from the cluster"),
        Err(e) => tracing::warn!(pod_id, error = %e, "couldn't close the record of a gone pod"),
    }
}

/// Marks every command still `running` under any of this pod's terminals
/// `'lost'` (no real exit code to report — see
/// `db::mark_terminal_command_lost`), and every one of the pod's live
/// terminals terminated — a dead agent was hosting all of them, not just
/// one. A safe no-op if the pod had no terminals.
async fn handle_crash_cleanup(pool: &PgPool, pod_id: i64, reason: Option<String>) {
    let (conversation_id, found_live_terminal) = close_pod_terminals(pool, pod_id).await;
    if let Some(conversation_id) = conversation_id {
        // One pod-level notification, gated on the same "found at least one
        // live terminal" condition that already makes a redundant second call
        // (e.g. the pre-existing reactive path still firing after this one
        // already ran) a harmless no-op — no separate dedup state needed. See
        // SME-12's "Detection design": the reason string, when Kubernetes
        // gave one, is passed straight through rather than guessed at.
        let notice = found_live_terminal.then(|| match reason {
            Some(reason) => format!(
                "Sandbox pod {pod_id} stopped unexpectedly ({reason}); every terminal running in it is no longer available."
            ),
            None => format!(
                "Sandbox pod {pod_id} stopped unexpectedly; every terminal running in it is no longer available."
            ),
        });
        // Then the same active wake as a normal command exit (see
        // `handle_agent_message`'s "exit" branch) — a crash can leave a
        // command marked 'lost' with nobody proactively telling the model,
        // the identical gap. One wake covers whatever this pass just
        // marked lost; `wake_conversation`'s own no-op-when-nothing-
        // pending behavior makes this cheap even when nothing actually
        // changed. Detached, notice included: this can run synchronously
        // from *inside* an already-in-progress `run_turn`/`execute()` call
        // that's already holding `conversation_id`'s lock (e.g.
        // `run_terminal_command_tool` → `sandbox::send_command` →
        // `reconnect_if_needed` → here), and both the notice (saved only
        // between turns, see `save_notice_between_turns`) and the wake
        // take that same non-reentrant lock. See
        // SME-13.
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Some(notice) = notice {
                if let Err(e) = crate::api::chat::save_notice_between_turns(&pool, conversation_id, notice).await {
                    tracing::warn!(conversation_id, pod_id, error = %e, "couldn't save the pod crash notice");
                }
            }
            let _ = crate::api::chat::wake_conversation(&pool, conversation_id).await;
        });
    }
    deregister(pod_id);
}

/// Marks every command still running in `pod_id`'s terminals lost and
/// closes the terminals, publishing each closure for the UI. Returns the
/// pod's conversation (if it could be found) and whether it had any live
/// terminal. Shared by a crash and by the user stopping the pod.
async fn close_pod_terminals(pool: &PgPool, pod_id: i64) -> (Option<i64>, bool) {
    let conversation_id = db::sandbox_pod_conversation_id(pool, pod_id)
        .await
        .ok()
        .flatten();
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
    (conversation_id, found_live_terminal)
}

/// Opens a WebSocket to `pod_id`'s agent over `dialer` and starts the two
/// tasks that own it: one drains `outgoing` into the socket, the other
/// hands every agent message to `handle_agent_message` (output and exits
/// into `terminal_events`/`terminal_commands`, replies to their waiting
/// request). When the socket ends unexpectedly, the reader reconnects, or
/// confirms a crash. One connection per **pod**, shared by every terminal
/// it hosts — see SME-9's "Why N pods and N terminals."
async fn connect(
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
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the agent has to say hello once the WebSocket is open. It sends
/// it before anything else, so only an agent that predates the hello, or a
/// very slow link, takes longer.
const HELLO_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(5) };

/// Reads the agent's hello and returns its version, if smelt speaks its
/// major. Anything else first means an agent smelt can't talk to.
async fn read_hello<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Result<ProtocolVersion, ConnectError>
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

async fn handle_agent_message(pool: &PgPool, conn: &Arc<TerminalConnection>, text: &str) {
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
            // something *else* triggers a turn) — see
            // SME-13. Detached:
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

#[cfg(test)]
mod tests;

/// smelt's side of the agent connection against a fake agent on a loopback
/// port (SME-53): requests and their errors, the hello, and one connection
/// per pod. The real agent and cluster are `test_terminal_lifecycle_end_to_end`'s.
#[cfg(test)]
mod agent_connection_tests;
