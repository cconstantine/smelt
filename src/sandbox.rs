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
use serde::Deserialize;
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
const DEFAULT_RUNNING_WAIT_TIMEOUT_SECS: u64 = 30;

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
    /// the plan — not resolved, just surfaced rather than guessed at.
    ExistingPodNotRunning(String),
    // Only ever constructed by `Sandbox::exec`, which is itself only called
    // by the real-cluster tests below (see its own cfg) — production code
    // talks to the sandbox through `sandbox_agent`'s WebSocket protocol
    // instead, never this lower-level kube-exec path.
    #[cfg_attr(not(test), allow(dead_code))]
    Io(std::io::Error),
    WebSocket(tokio_tungstenite::tungstenite::Error),
    /// A `sandbox_pods`/`sandbox_terminals` query failed — see the plan's
    /// "How" on why pod/terminal identity is DB-backed this round.
    Db(sqlx::Error),
    /// `create_pod` refuses: this conversation already has a live pod. See
    /// SME-11's "One pod per conversation."
    PodAlreadyExists,
    InvalidMountPath(String),
    StartFailed(String),
    /// Writing the SSH keys and git config into a pod failed (SME-32).
    GitSetup(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Kube(e) => write!(f, "kubernetes API error: {e}"),
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

fn pods_api(client: &kube::Client) -> Api<Pod> {
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
        &["sh", "-c", r#"mkdir -p -m 700 "$1" 2>&1"#, "sh", &keys_dir],
        None,
    )
    .await?;
    if made.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't make {keys_dir}: {}",
            made.stdout.trim()
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
                r#"umask 077 && cat > "$1.new" && chmod "$2" "$1.new" && mv "$1.new" "$1" 2>&1"#,
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
                written.stdout.trim()
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
           done 2>&1"#,
        "sh",
        &keys_dir,
    ];
    prune.extend(keep);
    let pruned = exec_with(client, pod_name, "sandbox", &prune, None).await?;
    if pruned.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't remove deleted keys from {keys_dir}: {}",
            pruned.stdout.trim()
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
/// Verified against a real cluster, not assumed — see the plan.
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
        cpu: &str,
        volumes: &[db::SandboxVolume],
    ) -> Result<Sandbox, SandboxError> {
        // Limits double as requests, so the deployment's 8Gi default would
        // reserve 8Gi per test pod.
        let docker = DockerSidecar {
            memory: "512Mi".to_string(),
            cpu: "250m".to_string(),
            storage: PodStorage::Ephemeral,
        };
        self.create_with_docker(session_id, memory, cpu, &docker, volumes).await
    }

    /// `memory`/`cpu` are already-resolved values (the caller's own
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
        cpu: &str,
        docker: &DockerSidecar,
        volumes: &[db::SandboxVolume],
    ) -> Result<Sandbox, SandboxError> {
        self.create_with_running_timeout(
            session_id,
            memory,
            cpu,
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
        cpu: &str,
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
                    // is still an open question per the plan — not
                    // handled yet.
                    return Err(SandboxError::ExistingPodNotRunning(phase));
                }
                // Reuse: what makes an active conversation's sandbox
                // survive a smelt server restart, see the plan's "Restart
                // behavior" section.
                false
            }
            None => {
                ensure_volume_claims(&self.client, volumes).await?;
                pods.create(
                    &PostParams::default(),
                    &build_pod_spec(&name, memory, cpu, docker, volumes),
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
                pods.delete(&name, &immediate_delete_params()).await.ok();
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
        pods.delete(&sandbox.pod_name, &immediate_delete_params())
            .await?;
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
    std::env::var("SANDBOX_MEMORY_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8Gi".to_string())
}

/// `SANDBOX_CPU_LIMIT`, default `"1"` — see `default_memory_limit`.
fn default_cpu_limit() -> String {
    std::env::var("SANDBOX_CPU_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1".to_string())
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

/// `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS`, default `30` — same pattern as
/// `default_memory_limit`. How long `wait_for_running` waits for a pod to
/// reach `Running` before giving up with `SandboxError::Timeout`. The
/// homelab cluster's real scheduling latency is why this exists as a
/// tunable rather than a bare constant: a CPU-constrained CI runner
/// schedules pods measurably slower than a real cluster or a
/// resource-rich dev machine, and 30s — plenty in both of the latter —
/// isn't a reliable bound under CI's actual constraints. See
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

/// A pod's Docker sidecar: its own limits, which nested containers count
/// against, and where its data lives (SME-33).
#[derive(Debug, Clone)]
pub struct DockerSidecar {
    pub memory: String,
    pub cpu: String,
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

/// `SANDBOX_DOCKER_CPU_LIMIT`, default `"1"` — see `default_memory_limit`.
fn default_docker_cpu_limit() -> String {
    std::env::var("SANDBOX_DOCKER_CPU_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1".to_string())
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
fn workspace_pvc_name(conversation_id: i64) -> String {
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

/// Whether the pod's Docker sidecar restarted since `last_reported`
/// restarts, and if so its new restart count and the reason Kubernetes
/// gave for the last exit (e.g. `OOMKilled`).
fn docker_restart_to_report(pod: &Pod, last_reported: i32) -> Option<(i32, Option<String>)> {
    let docker = pod
        .status
        .as_ref()?
        .init_container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == "docker")?;
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

/// `watch_pods`' bookkeeping for Docker sidecar restarts: remembers each
/// pod's restart count in `seen`, and returns the pod id and reason for a
/// restart to report. A pod's first listing (`initial`, after smelt starts
/// or the watch reconnects) is only remembered, so a restart from before
/// isn't reported again.
fn note_docker_restarts(
    seen: &mut HashMap<i64, i32>,
    pod: &Pod,
    initial: bool,
) -> Option<(i64, Option<String>)> {
    let pod_id = watched_pod_id(pod)?;
    let last = seen.get(&pod_id).copied().unwrap_or(0);
    let (count, reason) = docker_restart_to_report(pod, last)?;
    seen.insert(pod_id, count);
    (!initial).then_some((pod_id, reason))
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
        if let Err(e) = crate::api::chat::save_notice_between_turns(&pool, conversation_id, notice).await {
            tracing::warn!(conversation_id, pod_id, error = %e, "couldn't save the Docker restart notice");
        }
        let _ = crate::api::chat::wake_conversation(&pool, conversation_id).await;
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
const START_DOCKERD_SCRIPT: &str = include_str!("../docker/sandbox/start-dockerd.sh");

/// `memory`/`cpu` are plain Kubernetes `Quantity` strings (`"8Gi"`, `"1"`)
/// — no app-side parsing or validation of the format; an invalid value is
/// rejected by the Kubernetes API itself when the pod is actually
/// created, surfacing back through `SandboxError::Kube` as an ordinary
/// error. Bounded from above by the `smelt-park` namespace's own
/// `LimitRange` (`k8s/smelt-park-rbac.yaml`), not by anything here.
fn build_pod_spec(
    name: &str,
    memory: &str,
    cpu: &str,
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
                    period_seconds: Some(1),
                    failure_threshold: Some(60),
                    ..Default::default()
                }),
                resources: Some(limits(&docker.memory, &docker.cpu)),
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
                resources: Some(limits(memory, cpu)),
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

fn limits(memory: &str, cpu: &str) -> ResourceRequirements {
    let mut limits = std::collections::BTreeMap::new();
    limits.insert("memory".to_string(), Quantity(memory.to_string()));
    limits.insert("cpu".to_string(), Quantity(cpu.to_string()));
    ResourceRequirements {
        limits: Some(limits),
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
            pods.delete(&name, &immediate_delete_params()),
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
            TerminalError::NoTerminal => {
                write!(
                    f,
                    "no such terminal (it doesn't exist, or its pod's agent is unreachable)"
                )
            }
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
            TerminalError::FileOperation(message) => write!(f, "{message}"),
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
/// SME-11's "Change detection, not just 'was it
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

/// One `grep` match — see SME-19.
#[derive(Debug, Clone, PartialEq)]
pub struct GrepMatch {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// One file `grep` excluded (too large, or not valid UTF-8) — reported
/// explicitly rather than silently dropped, per the plan's "Decisions from
/// review."
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

/// `glob`'s result. `total` is the match count the walk actually found, up
/// to its internal scan ceiling (`sandbox_agent`'s `MAX_GLOB_SCAN`) — not
/// just how many fit in this page. `scan_capped` is true only when that
/// ceiling itself was hit, distinct from ordinary pagination (`total`
/// larger than one page, `scan_capped: false`).
#[derive(Debug, Clone, PartialEq)]
pub struct GlobResult {
    pub paths: Vec<String>,
    pub total: usize,
    pub scan_capped: bool,
}

/// `grep`'s result — same `total`/`scan_capped` meaning as `GlobResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub total: usize,
    pub scan_capped: bool,
    pub skipped: Vec<SkippedFile>,
}

/// The parsed, successful body of a file-tool agent response — what a
/// pending `pending_file_requests` oneshot resolves to on success (an
/// `Err(String)` carries the agent's own error message instead, same as
/// `pending_acks`). One enum covering all six operations since they share
/// one correlation map (keyed by `request_id`, not tied to a
/// `terminal_id`).
#[derive(Debug, Clone, PartialEq)]
enum FileResponse {
    Read(FileContents),
    Written { hash: String },
    Edited { hash: String },
    Listed(Vec<DirEntry>),
    Globbed(GlobResult),
    Grepped(GrepResult),
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
    pending_file_requests:
        StdMutex<HashMap<String, tokio::sync::oneshot::Sender<Result<FileResponse, String>>>>,
    /// Resolved once, when the connection is first established (see
    /// `connect`) — lets `handle_agent_message` publish a
    /// `SandboxCommandUpdate` for every output line and completion without
    /// a per-line DB round trip. See
    /// SME-10.
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
    TERMINAL_CONNECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&pod_id)
        .cloned()
}

fn registry_contains(pod_id: i64) -> bool {
    TERMINAL_CONNECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&pod_id)
}

fn register(pod_id: i64, conn: Arc<TerminalConnection>) {
    TERMINAL_CONNECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pod_id, conn);
}

fn deregister(pod_id: i64) {
    TERMINAL_CONNECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&pod_id);
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
    let mut connections = TERMINAL_CONNECTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
/// `limits` override the deployment's defaults for just this one pod —
/// see `PodLimitOverrides`.
pub async fn create_pod(
    pool: &PgPool,
    conversation_id: i64,
    limits: PodLimitOverrides,
) -> Result<i64, SandboxError> {
    let existing = db::list_sandbox_pods(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    check_pod_guard(&existing)?;

    let (memory, cpu, docker) = limits.resolve(conversation_id);

    let manager = get();
    let row = db::create_sandbox_pod(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
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
        .create_with_docker(&row.id.to_string(), &memory, &cpu, &docker, &volumes)
        .await
    {
        Ok(sandbox) => {
            // Keys and the commit identity, before anything can run a git
            // command (SME-32). A pod without them would fail its first
            // push with a confusing ssh error, so a failure here is the
            // pod's failure.
            if let Err(e) = crate::git::install_into_pod(pool, row.id).await {
                let _ = manager.delete(sandbox).await;
                let _ = db::terminate_sandbox_pod(pool, row.id).await;
                return Err(SandboxError::GitSetup(e));
            }
            std::mem::forget(sandbox);
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

/// `create_pod`'s per-pod overrides of the deployment's limits: the
/// sandbox container's (`SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT`, see the
/// plan's "Per-pod limit overrides") and the Docker sidecar's
/// (`SANDBOX_DOCKER_*_LIMIT`, SME-33). Plain Kubernetes quantity strings,
/// validated by Kubernetes itself against the namespace's `LimitRange`.
#[derive(Debug, Clone, Default)]
pub struct PodLimitOverrides {
    pub memory: Option<String>,
    pub cpu: Option<String>,
    pub docker_memory: Option<String>,
    pub docker_cpu: Option<String>,
}

impl PodLimitOverrides {
    /// The sandbox container's memory and CPU, and the Docker sidecar on
    /// the conversation's own claim.
    fn resolve(self, conversation_id: i64) -> (String, String, DockerSidecar) {
        let docker = DockerSidecar {
            memory: self.docker_memory.unwrap_or_else(default_docker_memory_limit),
            cpu: self.docker_cpu.unwrap_or_else(default_docker_cpu_limit),
            storage: PodStorage::Conversation(conversation_id),
        };
        (
            self.memory.unwrap_or_else(default_memory_limit),
            self.cpu.unwrap_or_else(default_cpu_limit),
            docker,
        )
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

async fn pod_port_is_listening_with(
    pool: &PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
    client: impl FnOnce() -> kube::Client,
) -> Result<bool, TerminalError> {
    let stream = open_pod_port_with(pool, conversation_id, host, port, client).await?;
    Ok(stream_is_listening(stream).await)
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
async fn force_terminate_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<db::SandboxPod>, SandboxError> {
    deregister(pod_id);

    let manager = get();
    let name = pod_name(pod_id);
    let pods = pods_api(&manager.client);
    if pods.get_opt(&name).await?.is_some() {
        pods.delete(&name, &immediate_delete_params()).await?;
    }

    let row = db::terminate_sandbox_pod(pool, pod_id)
        .await
        .map_err(SandboxError::Db)?;
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
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id)
        .await?
        .ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let payload =
        serde_json::json!({"action": "command", "terminal_id": terminal_id.to_string(), "id": command_id, "command": command})
            .to_string();
    conn.outgoing
        .send(payload)
        .map_err(|_| TerminalError::NoTerminal)
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
    let pod_id = db::sandbox_terminal_pod_id(pool, terminal_id)
        .await?
        .ok_or(TerminalError::NoTerminal)?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let payload =
        serde_json::json!({"action": "signal", "terminal_id": terminal_id.to_string(), "id": command_id, "signal": signal})
            .to_string();
    conn.outgoing
        .send(payload)
        .map_err(|_| TerminalError::NoTerminal)
}

/// Deletes every pod that exists for this conversation, unconditionally
/// (unlike `terminate_pod`, this is a hard teardown on conversation
/// deletion, not a guarded API the model calls) — see the plan's
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

pub async fn teardown_conversation(pool: &PgPool, conversation_id: i64) {
    let manager = get();
    let pods = pods_api(&manager.client);
    let rows = db::list_sandbox_pods(pool, conversation_id)
        .await
        .unwrap_or_default();
    for row in rows {
        deregister(row.id);
        let name = pod_name(row.id);
        if let Ok(Some(_)) = pods.get_opt(&name).await {
            if let Err(e) = pods.delete(&name, &immediate_delete_params()).await {
                tracing::warn!(pod = %name, error = %e, "failed to delete pod during conversation teardown");
            }
        }
    }
    // After the pods: Kubernetes holds a claim until no pod mounts it.
    delete_conversation_pvcs(&manager.client, conversation_id).await;
}

/// Returns the existing registry entry for `pod_id` if there is one;
/// otherwise tries to (re)connect to that pod's agent. This is what makes
/// a smelt restart transparently reconnect to a still-healthy agent, *and*
/// what detects a crashed agent (pod exists, `Running`, but nothing
/// answers) — see the plan's "Agent crash recovery": cleanup only, never
/// touches the pod, and does not attempt to launch a fresh agent itself
/// (that's `ensure_pod_connection`'s job, used only by `create_terminal`).
async fn reconnect_if_needed(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Arc<TerminalConnection>, TerminalError> {
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
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Arc<TerminalConnection>, TerminalError>>
            + Send
            + '_,
    >,
> {
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

/// `create_terminal`'s connection step. The pod's agent is its own
/// `ENTRYPOINT`, so unlike before there's nothing to launch here — this is
/// "connect," never "inject and launch." A failure to connect here is
/// routine (the agent hasn't bound its port yet, or this is genuinely the
/// pod's first-ever connection attempt), not a crash signal, so this
/// deliberately bypasses `reconnect_or_confirm_crash`'s retry/force-
/// terminate machinery — that's for callers who already expect a
/// connection to exist, which isn't true here by design. Still worth a
/// short bounded retry (same `RECONNECT_ATTEMPTS`/`RECONNECT_BACKOFF`
/// shape `reconnect_or_confirm_crash` uses) rather than a single attempt:
/// `Running` and "the agent has bound its port" aren't quite the same
/// instant, even though they're much closer together now than when the
/// agent was injected and launched after the fact.
async fn ensure_pod_connection(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Arc<TerminalConnection>, TerminalError> {
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

    let mut last_err = None;
    for attempt in 0..RECONNECT_ATTEMPTS {
        match connect(&manager.client, pool.clone(), pod_id, &name).await {
            Ok(conn) => {
                register(pod_id, conn.clone());
                return Ok(conn);
            }
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < RECONNECT_ATTEMPTS {
                    tokio::time::sleep(RECONNECT_BACKOFF).await;
                }
            }
        }
    }
    Err(last_err.expect("loop runs at least once").into())
}

/// Sends a `create_terminal`/`terminate_terminal` protocol action and
/// blocks (up to `ACK_TIMEOUT`) for its ack — see the plan's "Request/ack
/// correlation, new this round." `send_command`/`send_signal` don't go
/// through this; they stay fire-and-forget.
async fn request_terminal_action(
    conn: &Arc<TerminalConnection>,
    terminal_id: i64,
    action: &str,
) -> Result<(), TerminalError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    conn.pending_acks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(terminal_id, tx);

    let payload =
        serde_json::json!({"action": action, "terminal_id": terminal_id.to_string()}).to_string();
    if conn.outgoing.send(payload).is_err() {
        conn.pending_acks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&terminal_id);
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
            conn.pending_acks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&terminal_id);
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
    format!(
        "freq-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
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
    conn.pending_file_requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(request_id.clone(), tx);

    if conn.outgoing.send(payload.to_string()).is_err() {
        conn.pending_file_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_id);
        return Err(TerminalError::NoTerminal);
    }

    match tokio::time::timeout(ACK_TIMEOUT, rx).await {
        Ok(Ok(Ok(response))) => Ok(response),
        Ok(Ok(Err(message))) => Err(TerminalError::FileOperation(message)),
        Ok(Err(_)) => Err(TerminalError::NoTerminal), // sender dropped — connection ended before the response arrived
        Err(_) => {
            conn.pending_file_requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&request_id);
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
        _ => Err(TerminalError::FileOperation(
            "agent returned an unexpected response type for read_file".to_string(),
        )),
    }
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
        _ => Err(TerminalError::FileOperation(
            "agent returned an unexpected response type for write_file".to_string(),
        )),
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
        _ => Err(TerminalError::FileOperation(
            "agent returned an unexpected response type for edit_file".to_string(),
        )),
    }
}

/// Lists `path` (one level, non-recursive) in this conversation's pod.
pub async fn list_directory(
    pool: &PgPool,
    conversation_id: i64,
    path: &str,
) -> Result<Vec<DirEntry>, TerminalError> {
    let pod_id = conversation_pod_id(pool, conversation_id).await?;
    let conn = reconnect_if_needed(pool, pod_id).await?;
    let request_id = generate_request_id();
    let payload =
        serde_json::json!({"action": "list_directory", "request_id": request_id, "path": path});
    match request_file_action(&conn, payload, request_id).await? {
        FileResponse::Listed(entries) => Ok(entries),
        _ => Err(TerminalError::FileOperation(
            "agent returned an unexpected response type for list_directory".to_string(),
        )),
    }
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
    let request_id = generate_request_id();
    let payload = serde_json::json!({
        "action": "glob",
        "request_id": request_id,
        "path": path,
        "pattern": pattern,
        "offset": offset,
        "limit": limit,
    });
    match request_file_action(&conn, payload, request_id).await? {
        FileResponse::Globbed(result) => Ok(result),
        _ => Err(TerminalError::FileOperation(
            "agent returned an unexpected response type for glob".to_string(),
        )),
    }
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
    let request_id = generate_request_id();
    let payload = serde_json::json!({
        "action": "grep",
        "request_id": request_id,
        "path": path,
        "pattern": pattern,
        "glob": glob,
        "case_insensitive": case_insensitive,
        "offset": offset,
        "limit": limit,
    });
    match request_file_action(&conn, payload, request_id).await? {
        FileResponse::Grepped(result) => Ok(result),
        _ => Err(TerminalError::FileOperation(
            "agent returned an unexpected response type for grep".to_string(),
        )),
    }
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
    // Each pod's Docker sidecar restart count, see `note_docker_restarts`.
    let mut docker_restarts = HashMap::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(watcher::Event::Init) => listed.clear(),
            Ok(watcher::Event::InitApply(pod)) => {
                note_docker_restarts(&mut docker_restarts, &pod, true);
                if let Some(pod_id) = watched_pod_id(&pod) {
                    if !pod_has_finished(&pod) {
                        listed.insert(pod_id);
                    }
                }
            }
            Ok(watcher::Event::InitDone) => {
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
                if let Some(pod_id) = watched_pod_id(&pod) {
                    close_after_grace(pool.clone(), pod_id);
                }
            }
            Ok(watcher::Event::Apply(pod)) => {
                if let Some((pod_id, reason)) = note_docker_restarts(&mut docker_restarts, &pod, false) {
                    report_docker_restart(&pool, pod_id, reason).await;
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
        // the plan's "Detection design": the reason string, when Kubernetes
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
            tracing::info!(
                pod_id,
                "pod connection ended unexpectedly — attempting to reconnect or confirm a crash"
            );
            let _ = reconnect_or_confirm_crash(&pool, pod_id).await;
        } else {
            tracing::info!(
                pod_id,
                "pod connection ended (already torn down or replaced)"
            );
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

/// Mirrors `sandbox_agent`'s `GrepMatchInfo` — see
/// SME-19.
#[derive(Deserialize)]
struct AgentGrepMatch {
    path: String,
    line: u32,
    text: String,
}

/// Mirrors `sandbox_agent`'s `SkippedFileInfo`.
#[derive(Deserialize)]
struct AgentSkippedFile {
    path: String,
    reason: String,
}

/// Flexible enough to cover every message shape `sandbox_agent`'s tagged
/// `ServerMessage` enum serializes to — a line/exit event names `id`;
/// a terminal-action ack names `terminal_id` (and, on failure, `message`);
/// a file-tool response names `request_id` instead, plus whichever of
/// `lines`/`total_lines`/`hash`/`entries`/`paths`/`matches`/`skipped`/
/// `total`/`scan_capped` its `event` variant carries.
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
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    matches: Option<Vec<AgentGrepMatch>>,
    #[serde(default)]
    skipped: Option<Vec<AgentSkippedFile>>,
    #[serde(default)]
    total: Option<usize>,
    #[serde(default)]
    scan_capped: Option<bool>,
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
                Ok(FileResponse::Written {
                    hash: msg.hash.unwrap_or_default(),
                }),
            );
            return;
        }
        Some("file_edited") => {
            resolve_pending_file_request(
                conn,
                msg.request_id,
                Ok(FileResponse::Edited {
                    hash: msg.hash.unwrap_or_default(),
                }),
            );
            return;
        }
        Some("directory_listed") => {
            let entries = msg
                .entries
                .unwrap_or_default()
                .into_iter()
                .map(|e| DirEntry {
                    name: e.name,
                    is_dir: e.is_dir,
                    size: e.size,
                })
                .collect();
            resolve_pending_file_request(conn, msg.request_id, Ok(FileResponse::Listed(entries)));
            return;
        }
        Some("glob_matched") => {
            let result = GlobResult {
                paths: msg.paths.unwrap_or_default(),
                total: msg.total.unwrap_or(0),
                scan_capped: msg.scan_capped.unwrap_or(false),
            };
            resolve_pending_file_request(conn, msg.request_id, Ok(FileResponse::Globbed(result)));
            return;
        }
        Some("grep_matched") => {
            let result = GrepResult {
                matches: msg
                    .matches
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| GrepMatch {
                        path: m.path,
                        line: m.line,
                        text: m.text,
                    })
                    .collect(),
                total: msg.total.unwrap_or(0),
                scan_capped: msg.scan_capped.unwrap_or(false),
                skipped: msg
                    .skipped
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| SkippedFile {
                        path: s.path,
                        reason: s.reason,
                    })
                    .collect(),
            };
            resolve_pending_file_request(conn, msg.request_id, Ok(FileResponse::Grepped(result)));
            return;
        }
        Some("file_error") => {
            resolve_pending_file_request(
                conn,
                msg.request_id,
                Err(msg.message.unwrap_or_default()),
            );
            return;
        }
        _ => {}
    }

    if let (Some(id), Some(stream), Some(seq), Some(data)) = (
        msg.id.clone(),
        msg.stream.clone(),
        msg.seq,
        msg.data.clone(),
    ) {
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

fn resolve_pending_ack(
    conn: &Arc<TerminalConnection>,
    terminal_id: Option<String>,
    result: Result<(), String>,
) {
    let Some(terminal_id) = parse_terminal_id(&terminal_id) else {
        return;
    };
    if let Some(tx) = conn
        .pending_acks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&terminal_id)
    {
        let _ = tx.send(result);
    }
}

fn resolve_pending_file_request(
    conn: &Arc<TerminalConnection>,
    request_id: Option<String>,
    result: Result<FileResponse, String>,
) {
    let Some(request_id) = request_id else {
        return;
    };
    if let Some(tx) = conn
        .pending_file_requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&request_id)
    {
        let _ = tx.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_client() -> kube::Client {
        // Same call `main()` makes before building any TLS-using client
        // (see its own comment) — but `main()` never runs under `cargo
        // test`, so without this, the first `kube::Client` built here hits
        // rustls's auto-detect instead of an explicit choice. That
        // auto-detect only works when exactly one crypto-provider backend
        // is compiled into the binary; it started panicking
        // ("Could not automatically determine the process-level
        // CryptoProvider... make sure exactly one of the 'aws-lc-rs' and
        // 'ring' features is enabled") once `rmcp`'s own `reqwest` feature
        // (added for the MCP client, see Cargo.toml) pulled a second
        // candidate backend into the shared `reqwest` dependency. `let _
        // =` because a second call in the same test binary (multiple
        // tests each calling `test_client()`) is no-op-safe, not an error
        // worth surfacing.
        let _ = rustls::crypto::ring::default_provider().install_default();
        kube::Client::try_default()
            .await
            .expect("KUBECONFIG must point at a reachable cluster for sandbox tests")
    }

    fn fake_pod(id: i64) -> db::SandboxPod {
        db::SandboxPod {
            id,
            conversation_id: 1,
            created_at: chrono::Utc::now().naive_utc(),
            terminated_at: None,
        }
    }

    fn docker_for_conversation(conversation_id: i64) -> DockerSidecar {
        DockerSidecar {
            memory: "2Gi".to_string(),
            cpu: "2".to_string(),
            storage: PodStorage::Conversation(conversation_id),
        }
    }

    fn container<'a>(containers: &'a Option<Vec<Container>>, name: &str) -> &'a Container {
        containers
            .as_ref()
            .and_then(|cs| cs.iter().find(|c| c.name == name))
            .unwrap_or_else(|| panic!("pod spec should have a `{name}` container"))
    }

    fn mount_path_of<'a>(c: &'a Container, volume: &str) -> Option<&'a str> {
        c.volume_mounts
            .as_ref()?
            .iter()
            .find(|m| m.name == volume)
            .map(|m| m.mount_path.as_str())
    }

    #[test]
    fn test_pod_spec_runs_dockerd_in_a_privileged_native_sidecar() {
        let pod = build_pod_spec("sandbox-1", "1Gi", "1", &docker_for_conversation(42), &[]);
        let spec = pod.spec.expect("pod should have a spec");
        let docker = container(&spec.init_containers, "docker");

        // A native sidecar: an init container that keeps running, and that
        // Kubernetes restarts on its own when it dies (an OOM kill).
        assert_eq!(docker.restart_policy.as_deref(), Some("Always"));
        assert_eq!(
            docker.security_context.as_ref().and_then(|s| s.privileged),
            Some(true)
        );
        // Our own start script, never the image's entrypoint, which
        // rearranges the node's root cgroup (see START_DOCKERD_SCRIPT).
        let command = docker.command.as_ref().expect("sidecar should override the entrypoint");
        assert_eq!(command[..2], ["sh".to_string(), "-c".to_string()]);
        assert_eq!(command[2], START_DOCKERD_SCRIPT);
        let args = docker.args.as_ref().expect("sidecar should pass dockerd args");
        assert!(
            args.contains(&"--host=unix:///run/docker-sock/docker.sock".to_string()),
            "dockerd should listen on the shared socket: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.contains("tcp://")),
            "dockerd must never listen on TCP: {args:?}"
        );
        let limits = docker
            .resources
            .as_ref()
            .and_then(|r| r.limits.as_ref())
            .expect("sidecar should have limits");
        assert_eq!(limits.get("memory"), Some(&Quantity("2Gi".to_string())));
        assert_eq!(limits.get("cpu"), Some(&Quantity("2".to_string())));
        assert!(docker.startup_probe.is_some(), "the sandbox should wait until dockerd answers");
    }

    #[test]
    fn test_pod_spec_leaves_the_sandbox_container_unprivileged() {
        let pod = build_pod_spec("sandbox-1", "1Gi", "1", &docker_for_conversation(42), &[]);
        let spec = pod.spec.expect("pod should have a spec");
        let main = Some(spec.containers);
        let sandbox = container(&main, "sandbox");
        assert_ne!(
            sandbox.security_context.as_ref().and_then(|s| s.privileged),
            Some(true)
        );
    }

    #[test]
    fn test_pod_spec_shares_workspace_and_socket_but_keeps_docker_data_in_the_sidecar() {
        let pod = build_pod_spec("sandbox-1", "1Gi", "1", &docker_for_conversation(42), &[]);
        let spec = pod.spec.expect("pod should have a spec");
        let docker = container(&spec.init_containers, "docker");
        let main = Some(spec.containers.clone());
        let sandbox = container(&main, "sandbox");

        for c in [docker, sandbox] {
            assert_eq!(mount_path_of(c, "workspace"), Some("/workspace"), "{}", c.name);
            assert_eq!(mount_path_of(c, "docker-sock"), Some("/run/docker-sock"), "{}", c.name);
        }
        assert_eq!(mount_path_of(docker, "docker-data"), Some("/var/lib/docker"));
        assert_eq!(mount_path_of(sandbox, "docker-data"), None);
        // The sidecar (root, started first) hands /workspace to the sandbox user.
        let owner = docker
            .env
            .as_ref()
            .and_then(|env| env.iter().find(|e| e.name == "WORKSPACE_OWNER"))
            .and_then(|e| e.value.as_deref());
        assert_eq!(owner, Some(SANDBOX_OWNER));

        let volumes = spec.volumes.expect("pod should have volumes");
        let data = volumes.iter().find(|v| v.name == "docker-data").expect("docker-data volume");
        assert_eq!(
            data.persistent_volume_claim.as_ref().map(|p| p.claim_name.as_str()),
            Some("sandbox-docker-42")
        );
        // /workspace outlives the pod on a claim of its own (SME-32); the
        // socket dies with it.
        let workspace = volumes.iter().find(|v| v.name == "workspace").expect("workspace volume");
        assert_eq!(
            workspace.persistent_volume_claim.as_ref().map(|p| p.claim_name.as_str()),
            Some("sandbox-workspace-42")
        );
        let sock = volumes.iter().find(|v| v.name == "docker-sock").expect("docker-sock volume");
        assert!(sock.empty_dir.is_some(), "docker-sock should be an emptyDir");
    }

    #[test]
    fn test_pod_spec_ephemeral_storage_is_empty_dirs() {
        let docker = DockerSidecar {
            storage: PodStorage::Ephemeral,
            ..docker_for_conversation(42)
        };
        let pod = build_pod_spec("sandbox-1", "1Gi", "1", &docker, &[]);
        let volumes = pod.spec.and_then(|s| s.volumes).expect("pod should have volumes");
        for name in ["docker-data", "workspace"] {
            let v = volumes.iter().find(|v| v.name == name).expect(name);
            assert!(v.empty_dir.is_some(), "{name}");
            assert!(v.persistent_volume_claim.is_none(), "{name}");
        }
    }

    #[test]
    fn test_pod_spec_mounts_user_volumes_into_both_containers_at_the_same_path() {
        let volumes = vec![db::SandboxVolume {
            id: 7,
            name: "cache".to_string(),
            mount_path: "/data/cache".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }];
        let pod = build_pod_spec("sandbox-1", "1Gi", "1", &docker_for_conversation(42), &volumes);
        let spec = pod.spec.expect("pod should have a spec");
        let docker = container(&spec.init_containers, "docker");
        let main = Some(spec.containers.clone());
        let sandbox = container(&main, "sandbox");
        // So `docker run -v /data/cache:/c` sees the same files the model does.
        assert_eq!(mount_path_of(docker, "volume-7"), Some("/data/cache"));
        assert_eq!(mount_path_of(sandbox, "volume-7"), Some("/data/cache"));
    }

    #[test]
    fn test_pod_spec_labels_a_pod_with_its_conversation_only_when_it_uses_the_claim() {
        let labelled = build_pod_spec("sandbox-1", "1Gi", "1", &docker_for_conversation(42), &[]);
        assert_eq!(
            labelled.metadata.labels.as_ref().and_then(|l| l.get(CONVERSATION_LABEL)).map(String::as_str),
            Some("42"),
            "create_pod finds a conversation's still-stopping pods by this label"
        );
        let ephemeral = DockerSidecar {
            storage: PodStorage::Ephemeral,
            ..docker_for_conversation(42)
        };
        let unlabelled = build_pod_spec("sandbox-1", "1Gi", "1", &ephemeral, &[]);
        assert!(unlabelled.metadata.labels.and_then(|l| l.get(CONVERSATION_LABEL).cloned()).is_none());
    }

    fn claim(name: &str, conversation: Option<&str>) -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: conversation
                    .map(|c| [(CONVERSATION_LABEL.to_string(), c.to_string())].into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_orphaned_docker_claims_are_those_whose_conversation_is_gone() {
        let claims = vec![
            claim("sandbox-docker-1", Some("1")),
            claim("sandbox-docker-2", Some("2")),
            // A sandbox volume's claim has no conversation label.
            claim("sandbox-volume-3", None),
            claim("sandbox-docker-x", Some("not-a-number")),
        ];
        let live = std::collections::HashSet::from([1]);
        assert_eq!(orphaned_docker_claims(&claims, &live), vec![2]);
        // A conversation has two claims (Docker data and /workspace): it's
        // named once.
        let both = vec![
            claim("sandbox-docker-4", Some("4")),
            claim("sandbox-workspace-4", Some("4")),
        ];
        assert_eq!(orphaned_docker_claims(&both, &live), vec![4]);
    }

    #[test]
    fn test_pod_limit_overrides_replace_only_the_limits_they_name() {
        let overrides = PodLimitOverrides {
            memory: Some("3Gi".to_string()),
            docker_cpu: Some("4".to_string()),
            ..Default::default()
        };
        let (memory, cpu, docker) = overrides.resolve(7);
        assert_eq!(memory, "3Gi");
        assert_eq!(cpu, default_cpu_limit());
        assert_eq!(docker.memory, default_docker_memory_limit());
        assert_eq!(docker.cpu, "4");
        assert_eq!(docker.storage, PodStorage::Conversation(7));
    }

    fn pod_with_docker_status(restarts: i32, last_reason: Option<&str>) -> Pod {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };
        Pod {
            status: Some(PodStatus {
                init_container_statuses: Some(vec![ContainerStatus {
                    name: "docker".to_string(),
                    restart_count: restarts,
                    last_state: last_reason.map(|reason| ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            reason: Some(reason.to_string()),
                            exit_code: 137,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_docker_restart_to_report_ignores_a_pod_whose_docker_never_restarted() {
        assert_eq!(docker_restart_to_report(&Pod::default(), 0), None);
        assert_eq!(docker_restart_to_report(&pod_with_docker_status(0, None), 0), None);
    }

    #[test]
    fn test_docker_restart_to_report_gives_the_new_count_and_kubernetes_reason() {
        assert_eq!(
            docker_restart_to_report(&pod_with_docker_status(1, Some("OOMKilled")), 0),
            Some((1, Some("OOMKilled".to_string())))
        );
        assert_eq!(
            docker_restart_to_report(&pod_with_docker_status(3, Some("Error")), 2),
            Some((3, Some("Error".to_string())))
        );
    }

    #[test]
    fn test_docker_restart_to_report_reports_each_restart_once() {
        assert_eq!(docker_restart_to_report(&pod_with_docker_status(1, Some("OOMKilled")), 1), None);
    }

    fn named(mut pod: Pod, name: &str) -> Pod {
        pod.metadata.name = Some(name.to_string());
        pod
    }

    #[test]
    fn test_docker_restarts_are_reported_once_and_never_from_the_first_listing() {
        let mut seen = HashMap::new();
        // A restart from before smelt started, seen in the first listing.
        let old = named(pod_with_docker_status(2, Some("Error")), "sandbox-7");
        assert_eq!(note_docker_restarts(&mut seen, &old, true), None);
        assert_eq!(note_docker_restarts(&mut seen, &old, false), None, "already known");

        let again = named(pod_with_docker_status(3, Some("OOMKilled")), "sandbox-7");
        assert_eq!(
            note_docker_restarts(&mut seen, &again, false),
            Some((7, Some("OOMKilled".to_string())))
        );
        assert_eq!(note_docker_restarts(&mut seen, &again, false), None, "reported once");

        // Not smelt's pod.
        let other = named(pod_with_docker_status(1, Some("OOMKilled")), "something-else");
        assert_eq!(note_docker_restarts(&mut seen, &other, false), None);
    }

    /// DB-only: the notice lands in the pod's conversation, saying what
    /// was lost and what wasn't. The wake after it is pointed at a port
    /// nothing listens on.
    #[sqlx::test]
    async fn test_report_docker_restart_tells_the_pods_conversation(pool: PgPool) {
        let _anthropic_guard = crate::anthropic::test_support::lock_anthropic_base_url();
        unsafe {
            std::env::set_var("ANTHROPIC_BASE_URL", "http://127.0.0.1:1");
            std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        }
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create pod row");

        report_docker_restart(&pool, pod.id, Some("OOMKilled".to_string())).await;

        let saved = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let messages = db::list_messages(&pool, conversation.id).await.expect("list messages");
                if let Some(text) = messages.iter().find_map(|m| {
                    m.blocks().ok()?.into_iter().find_map(|b| match b {
                        crate::anthropic::ContentBlock::Text { text } if text.contains("Docker") => {
                            Some(text)
                        }
                        _ => None,
                    })
                }) {
                    return text;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("a Docker notice should be saved to the conversation");
        assert!(saved.contains("OOMKilled"), "{saved}");
        assert!(saved.contains("/workspace"), "should say what survived: {saved}");
    }

    #[test]
    fn test_check_reachable_port_refuses_both_of_the_agents_ports() {
        assert!(matches!(check_reachable_port(AGENT_PORT), Err(TerminalError::AgentPort(8088))));
        assert!(matches!(check_reachable_port(RELAY_PORT), Err(TerminalError::AgentPort(8089))));
        assert!(check_reachable_port(3000).is_ok());
    }

    #[test]
    fn test_pod_container_limits_include_the_docker_sidecar() {
        let pod = build_pod_spec("sandbox-1", "1Gi", "1", &docker_for_conversation(42), &[]);
        assert_eq!(
            pod_container_limits(&pod),
            (vec!["1Gi".to_string(), "2Gi".to_string()], vec!["1".to_string(), "2".to_string()])
        );
    }

    #[test]
    fn test_docker_pvc_spec_is_named_labelled_and_sized_for_the_conversation() {
        let [pvc, workspace] = conversation_pvc_specs(42);
        assert_eq!(workspace.metadata.name.as_deref(), Some("sandbox-workspace-42"));
        assert_eq!(workspace.metadata.labels, pvc.metadata.labels);
        assert_eq!(pvc.metadata.name.as_deref(), Some("sandbox-docker-42"));
        assert_eq!(
            pvc.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(CONVERSATION_LABEL))
                .map(String::as_str),
            Some("42"),
            "the startup sweep finds a claim's conversation by this label"
        );
        let spec = pvc.spec.expect("claim should have a spec");
        assert_eq!(spec.access_modes, Some(vec!["ReadWriteOnce".to_string()]));
        let requested = spec
            .resources
            .and_then(|r| r.requests)
            .and_then(|r| r.get("storage").cloned());
        assert_eq!(requested, Some(Quantity(default_docker_storage_size())));
    }

    #[test]
    fn test_resolve_mount_path_expands_bare_tilde() {
        assert_eq!(resolve_mount_path("~"), "/home/sandbox");
    }

    #[test]
    fn test_resolve_mount_path_expands_tilde_slash_prefix() {
        assert_eq!(resolve_mount_path("~/.ssh"), "/home/sandbox/.ssh");
    }

    #[test]
    fn test_resolve_mount_path_leaves_an_absolute_path_unchanged() {
        assert_eq!(resolve_mount_path("/data/cache"), "/data/cache");
    }

    #[test]
    fn test_resolve_mount_path_leaves_a_non_slash_tilde_prefix_unchanged() {
        // No `~otheruser` support — there's only ever one sandbox user —
        // so this passes through as a literal path rather than expanding.
        assert_eq!(resolve_mount_path("~foo"), "~foo");
    }

    #[test]
    fn test_volume_mounts_for_produces_a_pvc_backed_pair_per_volume() {
        let volumes = vec![
            db::SandboxVolume {
                id: 7,
                name: "ssh-key".to_string(),
                mount_path: "/home/sandbox/.ssh".to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
            },
            db::SandboxVolume {
                id: 12,
                name: "build-cache".to_string(),
                mount_path: "/data/cache".to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
            },
        ];

        let (vols, mounts) = volume_mounts_for(&volumes);
        assert_eq!(vols.len(), 2);
        assert_eq!(mounts.len(), 2);

        assert_eq!(vols[0].name, "volume-7");
        assert_eq!(
            vols[0]
                .persistent_volume_claim
                .as_ref()
                .expect("should be PVC-backed")
                .claim_name,
            "sandbox-volume-7"
        );
        assert_eq!(mounts[0].name, "volume-7");
        assert_eq!(mounts[0].mount_path, "/home/sandbox/.ssh");

        assert_eq!(vols[1].name, "volume-12");
        assert_eq!(
            vols[1]
                .persistent_volume_claim
                .as_ref()
                .expect("should be PVC-backed")
                .claim_name,
            "sandbox-volume-12"
        );
        assert_eq!(mounts[1].name, "volume-12");
        assert_eq!(mounts[1].mount_path, "/data/cache");
    }

    #[test]
    fn test_check_pod_guard_allows_when_no_live_pod_exists() {
        assert!(check_pod_guard(&[]).is_ok());
    }

    #[test]
    fn test_check_pod_guard_refuses_when_a_live_pod_already_exists() {
        let result = check_pod_guard(&[fake_pod(1)]);
        assert!(
            matches!(result, Err(SandboxError::PodAlreadyExists)),
            "expected PodAlreadyExists, got {result:?}"
        );
    }

    #[test]
    fn test_resolve_pod_id_returns_the_one_live_pod() {
        assert_eq!(resolve_pod_id(&[fake_pod(42)]).expect("should resolve"), 42);
    }

    #[test]
    fn test_resolve_pod_id_errors_with_no_pod_when_none_live() {
        let result = resolve_pod_id(&[]);
        assert!(
            matches!(result, Err(TerminalError::NoPod)),
            "expected NoPod, got {result:?}"
        );
    }

    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus,
        PodStatus,
    };

    fn pod_waiting(reason: &str, message: &str) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some("Pending".to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some(reason.to_string()),
                            message: Some(message.to_string()),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_pod_startup_failure_reports_a_container_that_cannot_start() {
        let failure = pod_startup_failure(&pod_waiting(
            "ImagePullBackOff",
            "Back-off pulling image \"smelt-sandbox:missing\"",
        ))
        .expect("a container stuck in ImagePullBackOff is a startup failure");
        assert!(failure.contains("ImagePullBackOff"), "got {failure}");
        assert!(failure.contains("Back-off pulling image"), "got {failure}");
    }

    /// What a pod waiting on a volume claim that doesn't exist actually
    /// reports (seen on this cluster): no container status yet, just an
    /// unschedulable `PodScheduled` condition.
    fn pod_unschedulable(message: &str) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some("Pending".to_string()),
                conditions: Some(vec![k8s_openapi::api::core::v1::PodCondition {
                    type_: "PodScheduled".to_string(),
                    status: "False".to_string(),
                    reason: Some("Unschedulable".to_string()),
                    message: Some(message.to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_pod_pending_detail_explains_an_unschedulable_pod() {
        let detail = pod_pending_detail(&pod_unschedulable(
            "0/1 nodes are available: persistentvolumeclaim \"sandbox-volume-7\" not found.",
        ))
        .expect("an unschedulable pod has a reason to report");
        assert!(detail.contains("persistentvolumeclaim"), "got {detail}");
    }

    #[test]
    fn test_pod_pending_detail_is_none_for_a_running_pod() {
        assert_eq!(pod_pending_detail(&pod_with_phase("Running")), None);
    }

    #[test]
    fn test_pod_startup_failure_is_none_while_a_pod_is_still_starting() {
        assert_eq!(pod_startup_failure(&pod_waiting("ContainerCreating", "")), None);
        assert_eq!(pod_startup_failure(&pod_with_phase("Pending")), None);
        assert_eq!(pod_startup_failure(&pod_with_phase("Running")), None);
    }

    fn pod_with_phase(phase: &str) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn failed_pod_with_container_state(
        state: Option<ContainerState>,
        last_state: Option<ContainerState>,
    ) -> Pod {
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
            terminated: Some(ContainerStateTerminated {
                reason: reason.map(str::to_string),
                ..Default::default()
            }),
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
        assert_eq!(
            decide_pod_death_reason(Some(pod)),
            Some(Some("OOMKilled".to_string()))
        );
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
        assert_eq!(
            decide_pod_death_reason(Some(pod)),
            Some(Some("Error".to_string()))
        );
    }

    #[test]
    fn test_decide_pod_death_reason_failed_falls_back_to_pod_status_reason_without_container_status()
     {
        let pod = Pod {
            status: Some(PodStatus {
                phase: Some("Failed".to_string()),
                reason: Some("Evicted".to_string()),
                container_statuses: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            decide_pod_death_reason(Some(pod)),
            Some(Some("Evicted".to_string()))
        );
    }

    #[test]
    fn test_decide_pod_death_reason_failed_with_no_reason_available_anywhere() {
        let pod = Pod {
            status: Some(PodStatus {
                phase: Some("Failed".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(decide_pod_death_reason(Some(pod)), Some(None));
    }

    #[test]
    fn test_decide_pod_death_reason_running_is_inconclusive() {
        assert_eq!(
            decide_pod_death_reason(Some(pod_with_phase("Running"))),
            None
        );
    }

    #[test]
    fn test_decide_pod_death_reason_pending_is_inconclusive() {
        assert_eq!(
            decide_pod_death_reason(Some(pod_with_phase("Pending"))),
            None
        );
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
    async fn test_create_pod_refuses_before_touching_the_manager_when_a_live_pod_exists(
        pool: PgPool,
    ) {
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");
        db::create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create sandbox pod");

        let result = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await;
        assert!(
            matches!(result, Err(SandboxError::PodAlreadyExists)),
            "expected PodAlreadyExists, got {result:?}"
        );
    }

    /// DB-only: with no live pod there's nothing to connect to, and the
    /// manager is never touched.
    #[sqlx::test]
    async fn test_open_pod_port_without_a_live_pod_is_no_pod(pool: PgPool) {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let result = open_pod_target(&pool, conversation.id, PodHost::Localhost, 3000).await;
        assert!(matches!(result, Err(TerminalError::NoPod)), "expected NoPod");
    }

    /// DB-only: the sandbox agent's port is refused before anything is
    /// looked up, so no route can reach it.
    #[sqlx::test]
    async fn test_open_pod_port_refuses_the_sandbox_agents_own_port(pool: PgPool) {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let result = open_pod_target(&pool, conversation.id, PodHost::Localhost, AGENT_PORT).await;
        assert!(matches!(result, Err(TerminalError::AgentPort(_))), "the agent's port must be refused");
    }

    /// Sends a bare HTTP request down `stream` and returns whatever comes
    /// back before the response stops arriving — never waits for the end
    /// of the stream, which a port-forward reports about a second late.
    async fn http_get_over(stream: &mut Box<dyn PodIo>, path: &str) -> String {
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("write the request");
        let mut buf = vec![0u8; 4096];
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
            Ok(Ok(n)) => String::from_utf8_lossy(&buf[..n]).to_string(),
            Ok(Err(e)) => panic!("read failed: {e}"),
            Err(_) => panic!("no response within 10s"),
        }
    }

    /// Real cluster: `open_pod_target` reaches a server inside the
    /// conversation's own pod — one bound to `127.0.0.1` there, as dev
    /// servers are by default — but never the sandbox agent's own port. A
    /// conversation with no pod of its own can't borrow another's, and a
    /// port nothing listens on ends at once with no bytes.
    #[sqlx::test]
    async fn test_open_pod_port_reaches_the_conversations_own_pod(pool: PgPool) {
        // Pod names are `sandbox-{pod_id}`, and every `#[sqlx::test]`
        // database restarts ids at 1 — start this one's ids far from the
        // low range `test_terminal_lifecycle_end_to_end` wipes and uses.
        let first_id = (uuid_like().parse::<u128>().unwrap() % 1_000_000_000) as i64 + 1_000_000;
        sqlx::query("SELECT setval(pg_get_serial_sequence('sandbox_pods', 'id'), $1)")
            .bind(first_id)
            .execute(&pool)
            .await
            .expect("move the pod id sequence");
        // Its own client and manager, never the process-global one: a
        // manager set here would die with this test's runtime and break
        // every later test that uses `get()` (seen as `Kube(Service(Closed))`
        // in `test_terminal_lifecycle_end_to_end`). The pod is created
        // under the name the database row gives it, as `create_pod` would.
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());

        let with_pod = db::create_conversation(&pool).await.expect("create conversation");
        let without_pod = db::create_conversation(&pool).await.expect("create conversation");
        let row = db::create_sandbox_pod(&pool, with_pod.id).await.expect("create the pod's row");
        let sandbox = manager
            .create(&row.id.to_string(), "128Mi", "250m", &[])
            .await
            .expect("create the pod");
        let open = |conversation_id: i64, port: u16| {
            let (client, pool) = (client.clone(), pool.clone());
            async move { open_pod_port_with(&pool, conversation_id, PodHost::Localhost, port, move || client).await }
        };
        // Python's server speaks HTTP/1.0 and closes after every response,
        // so the preview below also meets an upstream that's gone between
        // requests on one kept-alive connection.
        let started_server = sandbox
            .exec(&["sh", "-c", "setsid nohup python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 </dev/null &"])
            .await;

        // The server binds a moment after it starts. Everything is gathered
        // before asserting, so a failure still deletes the pod below
        // instead of leaving it in the cluster.
        let mut reply = Err("never tried".to_string());
        for _ in 0..50 {
            reply = match open(with_pod.id, 8000).await {
                Ok(mut stream) => Ok(http_get_over(&mut stream, "/no-such-path").await),
                Err(e) => Err(e.to_string()),
            };
            if reply.as_ref().is_ok_and(|r| !r.is_empty()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // The user's preview proxy over the same real port-forward: three
        // requests on one kept-alive connection. Each would take over a
        // second if anything waited for the port-forward's late end of
        // stream (see `open_pod_target`).
        let through_preview = {
            use crate::egress_proxy::{DialFuture, SandboxDial};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let preview_addr = listener.local_addr().expect("local addr");
            let template = crate::preview::PreviewTemplate::parse(&format!(
                "http://{{port}}-{{conversation}}.preview.localhost:{}",
                preview_addr.port()
            ))
            .expect("a template");
            let (dial_client, dial_pool) = (client.clone(), pool.clone());
            let dial_for: crate::preview::DialFor = Arc::new(move |conversation| {
                let (client, pool) = (dial_client.clone(), dial_pool.clone());
                Arc::new(move |_host, port| {
                    let (client, pool) = (client.clone(), pool.clone());
                    Box::pin(async move {
                        open_pod_port_with(&pool, conversation, PodHost::Localhost, port, move || client)
                            .await
                            .map_err(|e| e.to_string())
                    }) as DialFuture
                }) as SandboxDial
            });
            let server = tokio::spawn(crate::preview::serve(listener, template.clone(), dial_for));
            let http = reqwest::Client::builder()
                .resolve(&format!("8000-{}.preview.localhost", with_pod.id), preview_addr)
                .build()
                .expect("a client");
            let started = std::time::Instant::now();
            let mut statuses = Vec::new();
            for _ in 0..3 {
                match http.get(format!("{}/no-such-path", template.url_for(with_pod.id, PodHost::Localhost, 8000))).send().await {
                    Ok(response) => statuses.push(response.status().as_u16()),
                    Err(_) => statuses.push(0),
                }
            }
            server.abort();
            (statuses, started.elapsed())
        };
        let (probe_client, probe_pool) = (client.clone(), pool.clone());
        let server_listening =
            pod_port_is_listening_with(&probe_pool, with_pod.id, PodHost::Localhost, 8000, || probe_client.clone()).await;
        let agent_port_result = open(with_pod.id, AGENT_PORT).await;
        let unused_listening = pod_port_is_listening_with(&probe_pool, with_pod.id, PodHost::Localhost, 9, || probe_client.clone()).await;
        let without_pod_result = open(without_pod.id, 8000).await;
        let closed_reply = match open(with_pod.id, 9).await {
            Ok(mut stream) => Ok(http_get_over(&mut stream, "/").await),
            Err(e) => Err(e.to_string()),
        };
        pods_api(&client).delete(&pod_name(row.id), &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);

        started_server.expect("start a server in the pod");
        let reply = reply.expect("open the server's port");
        assert!(reply.starts_with("HTTP/1.0 404"), "expected the server's 404, got {reply:?}");
        assert!(
            matches!(agent_port_result, Err(TerminalError::AgentPort(_))),
            "the sandbox agent's port must be refused even with a pod"
        );
        assert!(
            matches!(without_pod_result, Err(TerminalError::NoPod)),
            "a conversation without a pod must not reach any pod"
        );
        assert_eq!(closed_reply.expect("open an unused port"), "", "nothing listens on port 9");
        assert!(server_listening.expect("probe the server's port"), "the server listens on 8000");
        assert!(!unused_listening.expect("probe port 9"), "nothing listens on port 9");
        let (statuses, elapsed) = through_preview;
        assert_eq!(statuses, vec![404, 404, 404], "the server's 404 through the preview each time");
        assert!(elapsed < Duration::from_secs(2), "three preview requests took {elapsed:?}");
    }

    #[test]
    fn test_validate_mount_path_accepts_absolute_paths() {
        for path in ["/data", "/home/dev/.cache", "/workspace/project"] {
            assert!(validate_mount_path(path).is_ok(), "{path} should be accepted");
        }
    }

    #[test]
    fn test_validate_mount_path_rejects_relative_empty_and_escaping_paths() {
        for path in ["relative/path", "data", "", "/", "/data/../etc", "../x"] {
            assert!(validate_mount_path(path).is_err(), "{path:?} should be rejected");
        }
    }

    /// A relative mount path is mounted into every sandbox pod, and a
    /// container can't start with one — so it must be refused before it's
    /// saved (and before the cluster is touched: MANAGER is unset here).
    #[sqlx::test]
    async fn test_create_volume_refuses_a_relative_mount_path_before_saving_it(pool: PgPool) {
        let result = create_volume(&pool, "cache", "relative/path").await;
        assert!(
            matches!(result, Err(SandboxError::InvalidMountPath(_))),
            "expected InvalidMountPath, got {result:?}"
        );
        assert!(
            db::list_sandbox_volumes(&pool).await.expect("list volumes").is_empty(),
            "the refused volume was saved anyway"
        );
    }

    /// DB-only — no MANAGER touch, since `conversation_pod_id` never calls
    /// `get()`.
    #[sqlx::test]
    async fn test_conversation_pod_id_resolves_the_live_pod_and_errors_with_no_pod_otherwise(
        pool: PgPool,
    ) {
        let conversation = db::create_conversation(&pool)
            .await
            .expect("create conversation");

        let before = conversation_pod_id(&pool, conversation.id).await;
        assert!(
            matches!(before, Err(TerminalError::NoPod)),
            "expected NoPod before any pod exists, got {before:?}"
        );

        let pod = db::create_sandbox_pod(&pool, conversation.id)
            .await
            .expect("create sandbox pod");
        let resolved = conversation_pod_id(&pool, conversation.id)
            .await
            .expect("should resolve");
        assert_eq!(resolved, pod.id);
    }

    fn unique_session_id(label: &str) -> String {
        format!("test-{label}-{}", uuid_like())
    }

    /// The `unique_session_id` label `test_terminal_lifecycle_end_to_end`'s
    /// own volume-mount pod uses — pulled out as a constant so the
    /// precheck below (which needs the *prefix*, sans the pod's own
    /// unpredictable nanosecond suffix) can't drift out of sync with the
    /// actual pod-creation call site.
    const VOLUME_MOUNT_SESSION_LABEL: &str = "volume-mount";

    /// A pod gets the user's SSH keys and commit identity where ssh and git
    /// read them, and a reinstall after a key is deleted removes it
    /// (SME-32).
    #[tokio::test]
    async fn test_git_files_reach_ssh_and_git_in_a_real_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let sandbox = manager
            .create(&unique_session_id("git-files"), "256Mi", "250m", &[])
            .await
            .expect("create pod");
        let pod_name = sandbox.pod_name.clone();

        let checks = tokio::time::timeout(Duration::from_secs(120), async {
            let key = crate::git::generate_key("test-key");
            let identity = crate::git::GitIdentity {
                name: "Ada \"Countess\" Lovelace".to_string(),
                email: "ada@example.com".to_string(),
            };
            let files =
                crate::git::pod_git_files(&[("test-key".to_string(), key.private_key.clone())], &identity);
            install_git_files(&client, &pod_name, &files)
                .await
                .expect("install git files");

            let mode = sandbox
                .exec(&["stat", "-c", "%a %U", "/etc/smelt/keys/test-key"])
                .await
                .expect("exec stat");
            assert_eq!(mode.stdout.trim(), "600 sandbox", "key file mode and owner");

            // The file is the key, byte for byte: ssh derives the same public half.
            let derived = sandbox
                .exec(&["ssh-keygen", "-y", "-f", "/etc/smelt/keys/test-key"])
                .await
                .expect("exec ssh-keygen");
            let expected: Vec<&str> = key.public_key.split(' ').take(2).collect();
            let got: Vec<&str> = derived.stdout.trim().split(' ').take(2).collect();
            assert_eq!(got, expected, "derived public key");

            let ssh = sandbox
                .exec(&["ssh", "-G", "github.com"])
                .await
                .expect("exec ssh -G");
            assert!(
                ssh.stdout.contains("identityfile /etc/smelt/keys/test-key"),
                "ssh -G github.com: {}",
                ssh.stdout
            );
            let known = sandbox
                .exec(&["ssh-keygen", "-F", "github.com", "-f", "/etc/ssh/ssh_known_hosts"])
                .await
                .expect("exec ssh-keygen -F");
            assert_eq!(known.exit_code, 0, "github.com is a known host: {}", known.stdout);

            let name = sandbox
                .exec(&["git", "config", "user.name"])
                .await
                .expect("exec git config");
            assert_eq!(name.stdout.trim(), "Ada \"Countess\" Lovelace");

            // Reinstalling (a key added elsewhere, the identity saved)
            // never leaves the key missing, even for a moment: a push
            // running then would fail (SME-32 code review 2, finding 5).
            let watch = sandbox.exec(&[
                "sh",
                "-c",
                "for i in $(seq 1 300); do [ -e /etc/smelt/keys/test-key ] || echo missing; sleep 0.01; done",
            ]);
            let reinstall = async {
                for _ in 0..3 {
                    install_git_files(&client, &pod_name, &files).await.expect("reinstall");
                }
            };
            let (watched, ()) = tokio::join!(watch, reinstall);
            let watched = watched.expect("exec watch");
            assert!(!watched.stdout.contains("missing"), "the key vanished during a reinstall");

            // The key is deleted: a reinstall without it removes the file.
            let files = crate::git::pod_git_files(&[], &identity);
            install_git_files(&client, &pod_name, &files)
                .await
                .expect("reinstall git files");
            let gone = sandbox
                .exec(&["test", "-e", "/etc/smelt/keys/test-key"])
                .await
                .expect("exec test");
            assert_eq!(gone.exit_code, 1, "a deleted key's file is removed");
            let ssh = sandbox
                .exec(&["ssh", "-G", "github.com"])
                .await
                .expect("exec ssh -G");
            assert!(!ssh.stdout.contains("/etc/smelt/keys/"), "{}", ssh.stdout);
        })
        .await;

        manager.delete(sandbox).await.expect("delete pod");
        checks.expect("checks finished within the timeout");
    }

    /// Sets up a bare repo at /tmp/origin.git inside `sandbox`'s pod, with
    /// `main` holding an AGENTS.md and a `feature` branch on top. Returns
    /// main's commit.
    async fn make_origin_repo(sandbox: &Sandbox) -> String {
        let script = r#"set -e
            git init -q -b main /tmp/src && cd /tmp/src
            printf 'Run make test before committing.\n' > AGENTS.md
            git add AGENTS.md && git -c user.name=t -c user.email=t@t commit -qm first
            git branch feature && git checkout -q feature
            printf 'x\n' > feature.txt && mkdir web && printf 'Use pnpm.\n' > web/AGENTS.md
            git add feature.txt web/AGENTS.md
            git -c user.name=t -c user.email=t@t commit -qm feature && git checkout -q main
            git clone -q --bare /tmp/src /tmp/origin.git
            git rev-parse main"#;
        let made = sandbox.exec(&["sh", "-c", script]).await.expect("exec make origin");
        assert_eq!(made.exit_code, 0, "make origin repo: {}", made.stdout);
        made.stdout.trim().to_string()
    }

    #[tokio::test]
    async fn test_clone_into_pod_checks_out_a_branch_and_reports_failures() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let sandbox = manager
            .create(&unique_session_id("git-clone"), "256Mi", "250m", &[])
            .await
            .expect("create pod");
        let pod_name = sandbox.pod_name.clone();

        let checks = tokio::time::timeout(Duration::from_secs(120), async {
            let main_commit = make_origin_repo(&sandbox).await;

            let cloned = crate::git::clone_into_pod(&client, &pod_name, "file:///tmp/origin.git", None, "origin", false, None)
                .await
                .expect("clone the default branch");
            assert_eq!(cloned.commit, main_commit);
            assert_eq!(cloned.branch, "main");
            let agents = sandbox
                .exec(&["cat", "/workspace/origin/AGENTS.md"])
                .await
                .expect("exec cat");
            assert_eq!(agents.stdout, "Run make test before committing.\n");

            let read = crate::git::read_agents_file(&client, &pod_name, "origin")
                .await
                .expect("read AGENTS.md")
                .expect("origin has an AGENTS.md");
            assert_eq!(read.content, "Run make test before committing.\n");
            assert_eq!(read.file_bytes, 33);
            assert_eq!(read.hash.len(), 64, "sha256 hex: {}", read.hash);
            assert!(read.nested.is_empty(), "main has no nested files: {:?}", read.nested);

            let feature = crate::git::clone_into_pod(
                &client,
                &pod_name,
                "file:///tmp/origin.git",
                Some("feature"),
                "origin-feature",
                false,
                None,
            )
            .await
            .expect("clone a branch");
            assert_eq!(feature.branch, "feature");
            assert_ne!(feature.commit, main_commit);
            let read = crate::git::read_agents_file(&client, &pod_name, "origin-feature")
                .await
                .expect("read AGENTS.md")
                .expect("feature has an AGENTS.md");
            assert_eq!(read.nested, vec!["web/AGENTS.md".to_string()]);

            let bare = sandbox
                .exec(&["git", "init", "-q", "/workspace/no-agents"])
                .await
                .expect("exec git init");
            assert_eq!(bare.exit_code, 0);
            let none = crate::git::read_agents_file(&client, &pod_name, "no-agents")
                .await
                .expect("read a repo without one");
            assert_eq!(none, None);

            // Bytes that aren't UTF-8 (or a 1 MiB cut through a character)
            // still load, with the bad bytes replaced (SME-32 code review,
            // finding 6).
            let bad = sandbox
                .exec(&["sh", "-c", "git init -q /workspace/bad-bytes && printf 'Use \\377 tabs.\\n' > /workspace/bad-bytes/AGENTS.md"])
                .await
                .expect("exec make bad bytes");
            assert_eq!(bad.exit_code, 0, "{}", bad.stderr);
            let read = crate::git::read_agents_file(&client, &pod_name, "bad-bytes")
                .await
                .expect("a file with invalid UTF-8 reads")
                .expect("it exists");
            assert_eq!(read.content, "Use \u{FFFD} tabs.\n");
            assert_eq!(read.file_bytes, 12);

            let missing = crate::git::clone_into_pod(&client, &pod_name, "file:///tmp/nope.git", None, "nope", false, None)
                .await
                .expect_err("a missing repo fails");
            assert!(missing.contains("does not appear to be a git repository"), "{missing}");

            // The directory is taken: git's own message, not a silent overwrite.
            let taken = crate::git::clone_into_pod(&client, &pod_name, "file:///tmp/origin.git", None, "origin", false, None)
                .await
                .expect_err("an existing directory fails");
            assert!(taken.contains("already exists"), "{taken}");

            // Retrying a failed clone clears what an interrupted one left
            // behind in the (persistent) directory (SME-32 code review,
            // finding 4).
            let partial = sandbox
                .exec(&["sh", "-c", "mkdir -p /workspace/partial/.git && echo half > /workspace/partial/half-written"])
                .await
                .expect("exec make partial");
            assert_eq!(partial.exit_code, 0);
            let replaced = crate::git::clone_into_pod(&client, &pod_name, "file:///tmp/origin.git", None, "partial", true, None)
                .await
                .expect("a retry replaces the leftover directory");
            assert_eq!(replaced.commit, main_commit);
            // The key a clone used is pinned in the checkout, so pushes
            // use it too (SME-32 code review 2, finding 3).
            let pinned_command = "ssh -i /etc/smelt/keys/b-repo -o IdentitiesOnly=yes";
            crate::git::clone_into_pod(&client, &pod_name, "file:///tmp/origin.git", None, "pinned", false, Some(pinned_command))
                .await
                .expect("clone with a key");
            let pinned = sandbox
                .exec(&["git", "-C", "/workspace/pinned", "config", "--local", "core.sshCommand"])
                .await
                .expect("exec git config");
            assert_eq!(pinned.stdout.trim(), pinned_command);
            let gone = sandbox
                .exec(&["test", "-e", "/workspace/partial/half-written"])
                .await
                .expect("exec test");
            assert_eq!(gone.exit_code, 1, "the leftover is gone");
        })
        .await;

        manager.delete(sandbox).await.expect("delete pod");
        checks.expect("checks finished within the timeout");
    }

    #[test]
    fn test_exec_with_no_status_is_a_failure_not_a_success() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
        // The connection closed before the API server said how it ended.
        assert_eq!(extract_exit_code(None), -1);
        // What a successful exec actually ends with: no causes at all.
        let success = Status {
            status: Some("Success".to_string()),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(Some(success)), 0);
    }

    // Not a real UUID — just enough entropy to avoid pod-name collisions
    // between concurrent test runs, without adding a `uuid` dependency.
    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// Docker in a real sandbox pod behaves like Docker on a Linux machine,
    /// and stays inside the pod (SME-33). One test so the pods, the claim
    /// and the base image are made once.
    #[tokio::test]
    async fn test_docker_in_a_sandbox_pod_works_and_stays_inside_the_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let pods = pods_api(&client);
        let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
            + 1_000_000_000;
        let docker = DockerSidecar {
            memory: "1Gi".to_string(),
            cpu: "500m".to_string(),
            storage: PodStorage::Conversation(conversation_id),
        };
        ensure_conversation_pvcs(&client, conversation_id).await.expect("ensure docker claim");
        // Every pod this test makes, so cleanup below finds them even after
        // a failed assertion unwinds out of the checks.
        let created: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

        let checks = tokio::time::timeout(Duration::from_secs(300), async {
            let first = manager
                .create_with_docker(&unique_session_id("docker"), "256Mi", "250m", &docker, &[])
                .await
                .expect("create pod with docker");
            created.lock().expect("created").push(first.pod_name.clone());

            // The very first command works: the pod only runs once dockerd answers.
            let info = first
                .exec(&["docker", "info", "--format", "{{.ServerVersion}}"])
                .await
                .expect("exec docker info");
            assert_eq!(info.exit_code, 0, "docker info right after create: {}", info.stdout);
            assert!(info.stdout.starts_with("29."), "server version: {}", info.stdout);

            // A base image from the sandbox's own files: no Docker Hub.
            let import = first
                .exec(&[
                    "bash",
                    "-c",
                    "sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null | docker import - local/base",
                ])
                .await
                .expect("exec import");
            assert_eq!(import.exit_code, 0, "import base image: {}", import.stdout);

            let run = first
                .exec(&["docker", "run", "--rm", "local/base", "echo", "hello-from-docker"])
                .await
                .expect("exec docker run");
            assert_eq!(run.stdout.trim(), "hello-from-docker");

            // A relative bind mount from /workspace sees the sandbox's files.
            let compose = first
                .exec(&[
                    "bash",
                    "-c",
                    "mkdir -p /workspace/proj/src && echo from-workspace > /workspace/proj/src/f \
                     && cd /workspace/proj \
                     && printf 'services:\\n  web:\\n    image: local/base\\n    command: [cat, /data/f]\\n    volumes: [./src:/data]\\n' > compose.yml \
                     && docker compose run --rm -q web 2>&1",
                ])
                .await
                .expect("exec compose");
            assert!(
                compose.stdout.contains("from-workspace"),
                "compose bind mount from /workspace: {}",
                compose.stdout
            );

            // Nested containers live under the sidecar's own cgroup, so its
            // memory limit applies, never at the node's cgroup root.
            let started = first
                .exec(&["docker", "run", "-d", "local/base", "sleep", "300"])
                .await
                .expect("exec docker run -d");
            let id = started.stdout.trim().to_string();
            let where_ = first
                .exec_in(
                    "docker",
                    &[
                        "sh",
                        "-c",
                        &format!(
                            "own=$(dirname $(sed -n 's/^0:://p' /proc/1/cgroup)); \
                             pid=$(docker --host={DOCKER_HOST} inspect -f '{{{{.State.Pid}}}}' {id}); \
                             echo \"$own\"; sed -n 's/^0:://p' /proc/$pid/cgroup"
                        ),
                    ],
                )
                .await
                .expect("exec in docker sidecar");
            let mut lines = where_.stdout.lines();
            let own = lines.next().unwrap_or_default().to_string();
            let nested = lines.next().unwrap_or_default().to_string();
            assert!(
                !own.is_empty() && nested.starts_with(&format!("{own}/docker/")),
                "nested container cgroup {nested:?} should be under the sidecar's {own:?}"
            );

            // Both browsers' routes into the pod (SME-33): a published port
            // on localhost, any container port at its address through the
            // agent's relay, and nothing outside the Docker range.
            let serve = first
                .exec(&[
                    "bash",
                    "-c",
                    "mkdir -p /workspace/www && echo from-a-container > /workspace/www/index.html \
                     && docker run -d -p 8000:8000 -v /workspace/www:/w -w /w local/base python3 -m http.server 8000 >/dev/null \
                     && docker run -d --name unpublished -v /workspace/www:/w -w /w local/base python3 -m http.server 8001 >/dev/null \
                     && sleep 2 && docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' unpublished",
                ])
                .await
                .expect("exec start servers");
            let container_ip: std::net::Ipv4Addr =
                serve.stdout.trim().parse().unwrap_or_else(|_| panic!("container ip: {:?}", serve.stdout));
            let fetch = |host: PodHost, port: u16| {
                let client = client.clone();
                let pod = first.pod_name.clone();
                async move {
                    use tokio::io::AsyncWriteExt;
                    let mut stream = dial_pod(&client, &pod, host, port).await.expect("dial_pod");
                    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.expect("send request");
                    let mut reply = String::new();
                    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut reply)).await;
                    reply
                }
            };
            assert!(fetch(PodHost::Localhost, 8000).await.contains("from-a-container"), "published port on localhost");
            assert!(
                fetch(PodHost::Container(container_ip), 8001).await.contains("from-a-container"),
                "unpublished container port through the relay"
            );
            assert_eq!(
                fetch(PodHost::Container(std::net::Ipv4Addr::new(10, 43, 0, 1)), 443).await,
                "",
                "the relay must refuse an address outside the Docker range"
            );
            // An address no container has yet (one still starting) isn't
            // listening: the relay mustn't look open while it's connecting.
            let nobody = dial_pod(&client, &first.pod_name, PodHost::Container(std::net::Ipv4Addr::new(172, 21, 200, 200)), 8001)
                .await
                .expect("dial_pod");
            assert!(!stream_is_listening(nobody).await, "nothing is at 172.21.200.200:8001");
            assert!(
                stream_is_listening(dial_pod(&client, &first.pod_name, PodHost::Container(container_ip), 8001).await.expect("dial_pod")).await,
                "the unpublished container is listening"
            );

            // A container can't reach the sandbox agent, whose WebSocket
            // runs commands with no login, at the bridge gateway.
            let agent = first
                .exec(&[
                    "docker",
                    "run",
                    "--rm",
                    "local/base",
                    "bash",
                    "-c",
                    &format!("</dev/tcp/{}/{AGENT_PORT}", DOCKER_BRIDGE_IP.split('/').next().unwrap_or_default()),
                ])
                .await
                .expect("exec agent probe");
            assert_ne!(agent.exit_code, 0, "a container reached the sandbox agent at the gateway");

            // dockerd never listens on TCP.
            for port in [2375, 2376] {
                let tcp = first
                    .exec(&["bash", "-c", &format!("</dev/tcp/127.0.0.1/{port}")])
                    .await
                    .expect("exec tcp probe");
                assert_ne!(tcp.exit_code, 0, "nothing should listen on {port}");
            }

            // Images live on the conversation's claim, so a new pod has them.
            let first_name = first.pod_name.clone();
            pods.delete(&first_name, &immediate_delete_params()).await.ok();
            std::mem::forget(first);
            while pods.get_opt(&first_name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let second = manager
                .create_with_docker(&unique_session_id("docker"), "256Mi", "250m", &docker, &[])
                .await
                .expect("recreate pod on the same claim");
            created.lock().expect("created").push(second.pod_name.clone());
            let images = second
                .exec(&["docker", "image", "ls", "-q", "local/base"])
                .await
                .expect("exec image ls");
            assert!(!images.stdout.trim().is_empty(), "local/base should survive a new pod");
            std::mem::forget(second);
        });
        let outcome = std::panic::AssertUnwindSafe(checks).catch_unwind().await;

        // Delete what this test made directly, not through the cleanup queue,
        // whether or not the checks passed.
        let created = created.into_inner().expect("created");
        for name in &created {
            pods.delete(name, &immediate_delete_params()).await.ok();
        }
        for name in &created {
            while pods.get_opt(name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        pvc_api(&client)
            .delete(&docker_pvc_name(conversation_id), &DeleteParams::default())
            .await
            .ok();
        match outcome {
            Ok(finished) => finished.expect("docker test should finish within the timeout"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// An OOM kill inside a nested container restarts only the Docker
    /// sidecar: the sandbox container, its processes and /workspace carry
    /// on, and the pod's status says why, as `docker_restart_to_report`
    /// reads it (SME-33's spike). Written memory, not `bytearray(n)`, whose
    /// untouched pages never count.
    #[tokio::test]
    async fn test_an_oom_in_a_nested_container_restarts_only_the_docker_sidecar() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let pods = pods_api(&client);
        let docker = DockerSidecar {
            memory: "512Mi".to_string(),
            cpu: "500m".to_string(),
            storage: PodStorage::Ephemeral,
        };
        let sandbox = manager
            .create_with_docker(&unique_session_id("docker-oom"), "128Mi", "250m", &docker, &[])
            .await
            .expect("create pod");
        let name = sandbox.pod_name.clone();

        let checks = std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(240), async {
            let setup = sandbox
                .exec(&[
                    "bash",
                    "-c",
                    "echo kept > /workspace/marker && (setsid sleep 1000 >/dev/null 2>&1 &) \
                     && sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null \
                        | docker import - local/base >/dev/null && echo ready",
                ])
                .await
                .expect("exec setup");
            assert_eq!(setup.stdout.trim(), "ready", "setup: {}", setup.stdout);
            // Detached: the container's memory outgrows the sidecar's limit.
            let started = sandbox
                .exec(&["docker", "run", "-d", "local/base", "sh", "-c", "head -c 900M /dev/zero | tail; sleep 600"])
                .await
                .expect("exec docker run");
            let container = started.stdout.trim().to_string();

            let restarted = loop {
                let pod = pods.get(&name).await.expect("get pod");
                if let Some(report) = docker_restart_to_report(&pod, 0) {
                    break (pod, report);
                }
                // A workload that exits on its own never reaches the limit:
                // say why now rather than at the timeout. Exit code 255 is
                // not that: it's how a restarted dockerd reports the
                // containers its OOM-killed predecessor ran, often before
                // the pod's status shows the restart.
                let state = sandbox
                    .exec(&["docker", "inspect", "-f", "{{.State.Status}} {{.State.ExitCode}}", &container])
                    .await
                    .expect("exec docker inspect");
                if state.stdout.trim().starts_with("exited") && state.stdout.trim() != "exited 255" {
                    let why = sandbox
                        .exec(&["sh", "-c", &format!("docker inspect -f '{{{{json .State}}}}' {container}; docker logs {container} 2>&1 | tail -5")])
                        .await
                        .expect("exec docker inspect");
                    panic!("the workload exited without an OOM kill: {}", why.stdout);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            };
            let (pod, report) = restarted;
            assert_eq!(report, (1, Some("OOMKilled".to_string())));
            let sandbox_restarts = pod
                .status
                .and_then(|s| s.container_statuses)
                .and_then(|cs| cs.into_iter().find(|c| c.name == "sandbox"))
                .map(|c| c.restart_count);
            assert_eq!(sandbox_restarts, Some(0), "the sandbox container must not restart");

            let after = sandbox
                .exec(&["bash", "-c", "cat /workspace/marker; pgrep -x sleep >/dev/null && echo sleep-alive || \
                        (for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = sleep ] && echo sleep-alive && break; done)"])
                .await
                .expect("exec after restart");
            assert!(after.stdout.contains("kept"), "/workspace should survive: {}", after.stdout);
            assert!(after.stdout.contains("sleep-alive"), "sandbox processes should survive: {}", after.stdout);
        }))
        .catch_unwind()
        .await;

        pods.delete(&name, &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);
        while pods.get_opt(&name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        match checks {
            Ok(finished) => finished.expect("the sidecar should be OOM-killed within the timeout"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// A new pod must not mount a conversation's Docker claim while the old
    /// one is still stopping: two dockerds on one /var/lib/docker corrupt
    /// it. The old pod gets a grace period so it lingers while terminating.
    #[tokio::test]
    async fn test_wait_for_conversation_pods_gone_waits_out_a_stopping_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let pods = pods_api(&client);
        let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
            + 1_000_000_000;
        let docker = DockerSidecar {
            memory: "256Mi".to_string(),
            cpu: "250m".to_string(),
            storage: PodStorage::Conversation(conversation_id),
        };
        ensure_conversation_pvcs(&client, conversation_id).await.expect("ensure docker claim");
        let sandbox = manager
            .create_with_docker(&unique_session_id("stopping"), "128Mi", "250m", &docker, &[])
            .await
            .expect("create pod");
        let name = sandbox.pod_name.clone();
        std::mem::forget(sandbox);

        pods.delete(&name, &DeleteParams { grace_period_seconds: Some(5), ..Default::default() })
            .await
            .expect("start deleting the pod");
        let waited =
            wait_for_conversation_pods_gone(&client, conversation_id, Duration::from_secs(60)).await;
        let still_there = pods.get_opt(&name).await.expect("get pod").is_some();

        pods.delete(&name, &immediate_delete_params()).await.ok();
        while pods.get_opt(&name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        pvc_api(&client).delete(&docker_pvc_name(conversation_id), &DeleteParams::default()).await.ok();

        assert!(waited.is_ok(), "the wait should finish once the pod is gone: {waited:?}");
        assert!(!still_there, "the wait returned while {name} was still stopping");
    }

    /// A conversation's Docker claim is created once and reused, and
    /// deleting it removes it. No pod: `local-path` binds on first use, so
    /// an unused claim stays `Pending`, which is fine here.
    #[tokio::test]
    async fn test_ensure_docker_pvc_creates_once_and_delete_removes_it() {
        let client = test_client().await;
        let pvcs = pvc_api(&client);
        // Far above any id a fresh test database hands out, and unique per run.
        let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
            + 1_000_000_000;
        let name = docker_pvc_name(conversation_id);

        ensure_conversation_pvcs(&client, conversation_id).await.expect("first ensure should create");
        let first = pvcs.get_opt(&name).await.expect("get claim");
        let first_uid = first.and_then(|p| p.metadata.uid);
        assert!(first_uid.is_some(), "ensure_docker_pvc should create {name}");

        ensure_conversation_pvcs(&client, conversation_id).await.expect("second ensure should reuse");
        let second_uid = pvcs.get_opt(&name).await.expect("get claim").and_then(|p| p.metadata.uid);
        assert_eq!(first_uid, second_uid, "a second ensure must reuse the claim, not replace it");

        delete_conversation_pvcs(&client, conversation_id).await;
        let gone = tokio::time::timeout(Duration::from_secs(30), async {
            while pvcs.get_opt(&name).await.expect("get claim").is_some() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await;
        // Delete directly too, so a failed assertion never leaves it behind.
        pvcs.delete(&name, &DeleteParams::default()).await.ok();
        assert!(gone.is_ok(), "delete_docker_pvc should remove {name}");
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
    /// SME-11's "One pod per conversation."
    #[sqlx::test]
    async fn test_terminal_lifecycle_end_to_end(pool: PgPool) {
        // Every terminal command that finishes during this test now
        // triggers a detached `chat::wake_conversation` call (see
        // SME-13) — without this
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
            pods_precheck
                .delete(&pod_name(n), &immediate_delete_params())
                .await
                .ok();
        }
        // Same reasoning as the pod-name wipe above, for
        // `create_volume`/`delete_volume`'s PVCs (`sandbox-volume-{id}`) —
        // this test's own `sandbox_volumes` row always lands on a
        // low integer id in a fresh `#[sqlx::test]` database, but a
        // previous run's PVC of the same k8s name can still be sitting
        // around if that run failed before reaching its own cleanup.
        let pvcs_precheck = pvc_api(&client);
        for n in 1..=30i64 {
            pvcs_precheck
                .delete(&sandbox_volume_pvc_name(n), &DeleteParams::default())
                .await
                .ok();
            pvcs_precheck
                .delete(&docker_pvc_name(n), &DeleteParams::default())
                .await
                .ok();
            pvcs_precheck
                .delete(&workspace_pvc_name(n), &DeleteParams::default())
                .await
                .ok();
        }
        // Same reasoning again, for the volume-mount pod itself
        // (`unique_session_id(VOLUME_MOUNT_SESSION_LABEL)`) — its name
        // carries a nanosecond suffix, not a low integer, so a fixed-range
        // wipe like the two above can't cover it; list and filter by
        // prefix instead. A previous run's pod here is what actually
        // starved the PVC precheck above of its point: `local-path`'s
        // `WaitForFirstConsumer` binding waits on *a* pod using the claim
        // reaching `Scheduled`, and a pile of these left `Pending` forever
        // (nothing schedules them — SandboxManager::create now cleans up
        // its own timeout, but this covers every prior run before that
        // fix, and any future failure mode that leaves one behind again)
        // starves that wait indefinitely. Safe to sweep unconditionally:
        // this is the only place in the whole suite that uses this label,
        // so nothing concurrently running can collide with it.
        let volume_mount_pod_prefix = format!("sandbox-test-{VOLUME_MOUNT_SESSION_LABEL}-");
        if let Ok(existing) = pods_precheck.list(&ListParams::default()).await {
            for pod in existing.items {
                if let Some(name) = pod.metadata.name.as_deref() {
                    if name.starts_with(&volume_mount_pod_prefix) {
                        pods_precheck
                            .delete(name, &immediate_delete_params())
                            .await
                            .ok();
                    }
                }
            }
        }

        let conversation_a = db::create_conversation(&pool)
            .await
            .expect("create conversation a");
        let conversation_b = db::create_conversation(&pool)
            .await
            .expect("create conversation b");
        let conversation_c = db::create_conversation(&pool)
            .await
            .expect("create conversation c");
        let conversation_d = db::create_conversation(&pool)
            .await
            .expect("create conversation d");

        let outcome = tokio::time::timeout(Duration::from_secs(400), async {
            // --- Guards fire before there's anything to guard against yet ---
            let too_early = create_terminal(&pool, conversation_a.id).await;
            assert!(
                matches!(too_early, Err(TerminalError::NoPod)),
                "create_terminal before any pod exists should fail with NoPod, got {too_early:?}"
            );

            // --- One pod per conversation: create_pod refuses a second
            // live pod, list reflects reality ---
            // A stored key and commit identity reach every new pod (SME-32).
            let key = crate::git::generate_key("lifecycle");
            db::create_ssh_key(&pool, "lifecycle", &key.public_key, &key.private_key)
                .await
                .expect("store key");
            db::set_git_identity(
                &pool,
                &crate::git::GitIdentity {
                    name: "Lifecycle Test".to_string(),
                    email: "lifecycle@example.com".to_string(),
                },
            )
            .await
            .expect("store identity");
            let pod_a = create_pod(&pool, conversation_a.id, PodLimitOverrides::default()).await.expect("create_pod (a) should succeed");
            let git_check = exec_with(
                &client,
                &pod_name(pod_a),
                "sandbox",
                &["sh", "-c", "test -s /etc/smelt/keys/lifecycle && git config user.email"],
                None,
            )
            .await
            .expect("exec git check");
            assert_eq!(
                (git_check.exit_code, git_check.stdout.trim()),
                (0, "lifecycle@example.com"),
                "a new pod has the stored key and identity"
            );
            // /workspace is the conversation's claim, whose root the
            // provisioner owns; the sandbox user gets it (SME-32 code
            // review 2, finding 6).
            let owner = exec_with(&client, &pod_name(pod_a), "sandbox", &["stat", "-c", "%U:%G", "/workspace"], None)
                .await
                .expect("exec stat");
            assert_eq!(owner.stdout.trim(), "sandbox:sandbox", "/workspace belongs to the sandbox user");

            // clone_repo records the repo and checks it out in the pod.
            let origin = exec_with(
                &client,
                &pod_name(pod_a),
                "sandbox",
                &["sh", "-c", "set -e; git init -q -b main /tmp/src; cd /tmp/src; echo 'Run make test.' > AGENTS.md; git add AGENTS.md; git commit -qm one; git clone -q --bare /tmp/src /tmp/origin.git; git rev-parse HEAD"],
                None,
            )
            .await
            .expect("exec make origin");
            assert_eq!(origin.exit_code, 0, "make origin: {}{}", origin.stdout, origin.stderr);
            let repo = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, None)
                .await
                .expect("clone_repo");
            assert_eq!(repo.path, "/workspace/origin");
            assert_eq!(repo.status, crate::git::RepoStatus::Ready);
            assert_eq!(repo.commit.as_deref(), Some(origin.stdout.trim()));
            assert_eq!(repo.branch.as_deref(), Some("main"));
            // Its AGENTS.md waits on the user: the model cloned a remote
            // they haven't decided about.
            assert_eq!(repo.instructions, crate::git::InstructionsState::AwaitingTrust);
            assert_eq!(repo.instructions_preview.as_deref(), Some("Run make test.\n"));
            assert!(crate::git::project_instructions(&pool, conversation_a.id).await.expect("loaded").is_empty());
            // Trusted, it's loaded into the model's context.
            crate::git::decide_trust(&pool, conversation_a.id, repo.id, true).await.expect("trust");
            let loaded = crate::git::project_instructions(&pool, conversation_a.id)
                .await
                .expect("project instructions");
            assert_eq!(loaded.len(), 1, "{loaded:?}");
            assert_eq!(loaded[0].content, "Run make test.\n");
            assert_eq!(loaded[0].path, "/workspace/origin");
            assert_eq!(loaded[0].commit, repo.commit);
            // A failed clone retried with another branch (or URL) clones
            // what's asked for now, not what failed (SME-32 code review,
            // finding 1).
            let failed = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", Some("nope"), Some("retry"))
                .await
                .expect_err("there's no branch nope");
            assert!(failed.contains("nope"), "{failed}");
            let retried = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, Some("retry"))
                .await
                .expect("the retry clones the default branch");
            assert_eq!(retried.requested_branch, None);
            assert_eq!(retried.branch.as_deref(), Some("main"));
            assert_eq!(retried.status, crate::git::RepoStatus::Ready);

            // A clone refused because the directory already holds work
            // doesn't delete that work when retried; only a clone that was
            // interrupted gets its directory replaced (SME-32 code review
            // 2, finding 1).
            let work = exec_with(
                &client,
                &pod_name(pod_a),
                "sandbox",
                &["sh", "-c", "mkdir -p /workspace/mywork && echo precious > /workspace/mywork/notes.txt"],
                None,
            )
            .await
            .expect("exec make work");
            assert_eq!(work.exit_code, 0, "{}", work.stderr);
            for attempt in ["first", "retry"] {
                let refused = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, Some("mywork"))
                    .await
                    .expect_err("the directory holds work");
                assert!(refused.contains("already exists"), "{attempt}: {refused}");
            }
            let kept = exec_with(&client, &pod_name(pod_a), "sandbox", &["cat", "/workspace/mywork/notes.txt"], None)
                .await
                .expect("exec cat");
            assert_eq!(kept.stdout, "precious\n", "a retry must not delete the work");
            let mywork = db::list_conversation_repos(&pool, conversation_a.id)
                .await
                .expect("list")
                .into_iter()
                .find(|r| r.dir == "mywork")
                .expect("the mywork repo");
            db::set_repo_failed(&pool, mywork.id, crate::git::CLONE_INTERRUPTED).await.expect("mark interrupted");
            let replaced = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, Some("mywork"))
                .await
                .expect("an interrupted clone's retry replaces its directory");
            assert_eq!(replaced.status, crate::git::RepoStatus::Ready);

            // Asking again returns the same checkout rather than a second clone.
            let again = crate::git::clone_repo(&pool, conversation_a.id, "file:///tmp/origin.git", None, None)
                .await
                .expect("clone_repo again");
            assert_eq!(again.id, repo.id);
            let wrote = exec_with(
                &client,
                &pod_name(pod_a),
                "sandbox",
                &["sh", "-c", "echo 'not pushed yet' > /workspace/origin/uncommitted.txt"],
                None,
            )
            .await
            .expect("exec write");
            assert_eq!(wrote.exit_code, 0, "{}", wrote.stderr);

            // AGENTS.md changes in the checkout: noticed, not reloaded.
            let edited = exec_with(
                &client,
                &pod_name(pod_a),
                "sandbox",
                &["sh", "-c", "echo 'Run make lint too.' >> /workspace/origin/AGENTS.md"],
                None,
            )
            .await
            .expect("exec edit");
            assert_eq!(edited.exit_code, 0, "{}", edited.stderr);
            crate::git::refresh_instructions(&pool, conversation_a.id).await;
            let repos = crate::git::list_repos(&pool, conversation_a.id).await.expect("list repos");
            assert_eq!(repos[0].instructions, crate::git::InstructionsState::Changed, "{repos:?}");
            let loaded = crate::git::project_instructions(&pool, conversation_a.id).await.expect("loaded");
            assert_eq!(loaded[0].content, "Run make test.\n", "still the loaded version");
            let duplicate = create_pod(&pool, conversation_a.id, PodLimitOverrides::default()).await;
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
            // SME-13). ANTHROPIC_API_KEY
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
            create_pod(&pool, conversation_b.id, PodLimitOverrides::default()).await.expect("create_pod (b) should succeed");
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

            // --- glob/grep: pattern-based file discovery and content
            // search — see SME-19. A fresh
            // subdirectory, kept separate from example.txt's own
            // (by-now-heavily-edited) content above so this section's
            // expectations don't depend on tracking that history. ---
            write_file(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep/one.rs", "fn one() {}\n", None)
                .await
                .expect("write_file should succeed");
            write_file(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep/nested/two.rs", "fn two() {}\n", None)
                .await
                .expect("write_file should create parent directories");
            write_file(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep/notes.txt", "just plain prose, nothing to match\n", None)
                .await
                .expect("write_file should succeed");

            let recursive_glob = glob(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep", "**/*.rs", 1, 50)
                .await
                .expect("glob should succeed");
            let mut recursive_paths = recursive_glob.paths.clone();
            recursive_paths.sort();
            assert_eq!(
                recursive_paths,
                vec![
                    "/tmp/file-tools-test/glob-grep/nested/two.rs".to_string(),
                    "/tmp/file-tools-test/glob-grep/one.rs".to_string(),
                ],
                "**/*.rs should match .rs files at every depth"
            );
            assert_eq!(recursive_glob.total, 2);
            assert!(!recursive_glob.scan_capped);

            let shallow_glob = glob(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep", "*.rs", 1, 50)
                .await
                .expect("glob should succeed");
            assert_eq!(
                shallow_glob.paths,
                vec!["/tmp/file-tools-test/glob-grep/one.rs".to_string()],
                "a bare * shouldn't cross into nested/ the way ** does"
            );

            let grep_result = grep(&pool, conversation_a.id, "/tmp/file-tools-test/glob-grep", "fn ", None, false, 1, 20)
                .await
                .expect("grep should succeed");
            let mut grep_paths: Vec<&str> = grep_result.matches.iter().map(|m| m.path.as_str()).collect();
            grep_paths.sort();
            assert_eq!(
                grep_paths,
                vec![
                    "/tmp/file-tools-test/glob-grep/nested/two.rs",
                    "/tmp/file-tools-test/glob-grep/one.rs",
                ],
                "grep should find matches across every file under path, not just the top level"
            );
            assert_eq!(grep_result.total, 2);
            assert!(!grep_result.scan_capped);
            assert!(grep_result.skipped.is_empty());

            let filtered_grep = grep(
                &pool,
                conversation_a.id,
                "/tmp/file-tools-test/glob-grep",
                "fn ",
                Some("nested/*.rs".to_string()),
                false,
                1,
                20,
            )
            .await
            .expect("grep should succeed");
            assert_eq!(
                filtered_grep.matches.len(), 1,
                "grep's glob filter should narrow the search to just the matching file"
            );
            assert_eq!(filtered_grep.matches[0].path, "/tmp/file-tools-test/glob-grep/nested/two.rs");

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
            let pod_c = create_pod(&pool, conversation_c.id, PodLimitOverrides::default()).await.expect("create_pod (c) should succeed");
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
            let pod_d = create_pod(&pool, conversation_d.id, PodLimitOverrides::default()).await.expect("create_pod (d) should succeed");
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

            // /workspace is the conversation's own: the next pod has the
            // checkout, uncommitted work included.
            let pod_a2 = create_pod(&pool, conversation_a.id, PodLimitOverrides::default())
                .await
                .expect("a second pod for conversation a");
            let kept = exec_with(
                &client,
                &pod_name(pod_a2),
                "sandbox",
                &["sh", "-c", "cat /workspace/origin/uncommitted.txt && git -C /workspace/origin status --porcelain"],
                None,
            )
            .await
            .expect("exec check workspace");
            assert_eq!(
                (kept.exit_code, kept.stdout.as_str()),
                (0, "not pushed yet\n M AGENTS.md\n?? uncommitted.txt\n"),
                "the checkout and its uncommitted file survive the pod: {}",
                kept.stderr
            );
            let repos = crate::git::list_repos(&pool, conversation_a.id).await.expect("list repos");
            assert_eq!(repos.len(), 3, "origin, its retry and mywork: {repos:?}");
            assert_eq!(repos[0].status, crate::git::RepoStatus::Ready, "{repos:?}");
            assert_eq!(repos[0].instructions, crate::git::InstructionsState::Changed, "still to reload: {repos:?}");
            assert_eq!(repos[1].status, crate::git::RepoStatus::Ready, "{repos:?}");
            terminate_pod(&pool, conversation_a.id).await.expect("terminate the second pod (a)");

            let pods_after = list_pods(&pool, conversation_a.id).await.expect("list_pods");
            assert!(pods_after.is_empty(), "no pods should be listed after terminating it, got {pods_after:?}");

            // "Work on a repo" on a conversation with no sandbox starts one.
            // The repo is recorded first, so a message sent while the
            // sandbox starts waits for the clone (SME-32 code review,
            // finding 3).
            let attaching = tokio::spawn({
                let pool = pool.clone();
                let id = conversation_a.id;
                async move { crate::git::attach_repo(&pool, id, "file:///tmp/missing.git", None).await }
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                !crate::git::wait_for_clones(&pool, conversation_a.id, Duration::ZERO).await,
                "a turn started while the sandbox starts should see the clone coming"
            );
            let attached = attaching
                .await
                .expect("attach task")
                .expect_err("this origin doesn't exist");
            assert!(attached.contains("does not appear to be a git repository"), "{attached}");
            // The user named it, so it counts as trusted.
            assert_eq!(
                db::get_repo_trust(&pool, "file/tmp/missing").await.expect("trust"),
                Some(true)
            );
            assert_eq!(
                list_pods(&pool, conversation_a.id).await.expect("list_pods").len(),
                1,
                "attach_repo started a sandbox"
            );
            terminate_pod(&pool, conversation_a.id).await.expect("terminate the attach pod (a)");
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
            // tool call — see SME-12. ---
            let conversation_e = db::create_conversation(&pool).await.expect("create conversation e");
            let pod_e = create_pod(&pool, conversation_e.id, PodLimitOverrides::default()).await.expect("create_pod (e) should succeed");
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
            create_pod(&pool, conversation_f.id, PodLimitOverrides::default()).await.expect("create_pod (f) should succeed");
            terminate_pod(&pool, conversation_f.id).await.expect("terminate_pod (f) should succeed");
            tokio::time::sleep(Duration::from_secs(2)).await; // give a wrongly-firing reader task a chance to misfire
            assert!(
                !any_message_contains(&pool, conversation_f.id, "stopped unexpectedly").await,
                "a deliberate terminate_pod must never produce a crash notification"
            );

            // --- The user stops a pod that has an open terminal and a
            // running command: everything is torn down, the command is
            // marked lost, and the model gets a user-stop notice, not a
            // crash notice. See SME-26. ---
            let conversation_g = db::create_conversation(&pool).await.expect("create conversation g");
            let mut app_events = events::subscribe_app();
            let pod_g = create_pod(&pool, conversation_g.id, PodLimitOverrides::default()).await.expect("create_pod (g) should succeed");
            assert!(
                received_pods_changed(&mut app_events).await,
                "creating a pod should tell app-wide listeners"
            );
            let terminal_g = create_terminal(&pool, conversation_g.id).await.expect("create_terminal (g) should succeed");
            db::create_terminal_command(&pool, conversation_g.id, terminal_g, "long-g", "sleep 300")
                .await
                .expect("create_terminal_command");
            send_command(&pool, terminal_g, "long-g", "sleep 300").await.expect("send_command");
            tokio::time::sleep(Duration::from_millis(300)).await;

            let overviews = crate::api::pods::pod_overviews(&pool).await.expect("pod_overviews");
            let overview_g = overviews
                .iter()
                .find(|o| o.pod_id == pod_g)
                .expect("the pods view should list pod (g)");
            assert_eq!(overview_g.conversation_id, conversation_g.id);
            assert_eq!(overview_g.status.as_deref(), Some("Running"));
            // The sandbox container's and the Docker sidecar's, added up (SME-33).
            assert_eq!(
                overview_g.memory_limit,
                crate::api::pods::sum_memory_limits(&[default_memory_limit(), default_docker_memory_limit()])
            );
            assert_eq!(
                overview_g.cpu_limit,
                crate::api::pods::sum_cpu_limits(&[default_cpu_limit(), default_docker_cpu_limit()])
            );
            assert_eq!(overview_g.terminals, 1);
            assert_eq!(overview_g.activity, crate::api::pods::PodActivity::Busy, "a command is running");
            // Live usage, end to end: metrics-server samples a new pod
            // within a minute or so, and smelt's RBAC lets it read that.
            let usage_g = tokio::time::timeout(Duration::from_secs(90), async {
                loop {
                    let overviews = crate::api::pods::pod_overviews(&pool).await.expect("pod_overviews");
                    if let Some(usage) = overviews.iter().find(|o| o.pod_id == pod_g).and_then(|o| o.usage.clone()) {
                        return usage;
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            })
            .await
            .expect("the pods view should show pod (g)'s live usage once metrics-server has sampled it");
            assert!(usage_g.memory_bytes > 0, "a running pod uses some memory: {usage_g:?}");

            stop_pod_for_user(&pool, pod_g).await.expect("stop_pod_for_user (g) should succeed");
            assert!(
                received_pods_changed(&mut app_events).await,
                "stopping a pod should tell app-wide listeners"
            );

            let command_g = db::get_terminal_command(&pool, "long-g").await.expect("get").expect("the command");
            assert_eq!(command_g.status, "lost", "a running command should be marked lost");
            assert!(
                list_terminals(&pool, conversation_g.id).await.expect("list_terminals").is_empty(),
                "the pod's terminal should be closed"
            );
            assert!(
                db::list_sandbox_pods(&pool, conversation_g.id).await.expect("list pods").is_empty(),
                "the pod should be marked terminated"
            );
            assert!(
                pods_api(&client).get_opt(&pod_name(pod_g)).await.expect("get pod").is_none_or(|p| p.metadata.deletion_timestamp.is_some()),
                "the Kubernetes pod should be gone or going"
            );
            let notified = tokio::time::timeout(Duration::from_secs(10), async {
                while !any_message_contains(&pool, conversation_g.id, "The user stopped sandbox pod").await {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
            assert!(notified.is_ok(), "the model should be told the user stopped the pod");
            assert!(
                !any_message_contains(&pool, conversation_g.id, "stopped unexpectedly").await,
                "a user stop must not be reported as a crash"
            );

            // --- Keeping records in step with the cluster (`watch_pods`):
            // a record whose pod vanished before the watch started is closed
            // when the watch's first listing completes; a pod deleted while
            // the watch runs is closed after the grace period; a young
            // record whose pod may still be starting is left alone. All
            // quietly. See SME-26. ---
            let updated_at = |id: i64| {
                let pool = pool.clone();
                async move {
                    sqlx::query_scalar::<_, chrono::NaiveDateTime>("SELECT updated_at FROM conversations WHERE id = $1")
                        .bind(id)
                        .fetch_one(&pool)
                        .await
                        .expect("the conversation's updated_at")
                }
            };
            let wait_until_closed = |pod_id: i64| {
                let pool = pool.clone();
                async move {
                    tokio::time::timeout(Duration::from_secs(30), async {
                        while db::sandbox_pod_is_live(&pool, pod_id).await.expect("is live") {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    })
                    .await
                    .is_ok()
                }
            };
            let wait_until_gone_from_kubernetes = |pod_id: i64| {
                let client = client.clone();
                async move {
                    tokio::time::timeout(Duration::from_secs(60), async {
                        while pods_api(&client).get_opt(&pod_name(pod_id)).await.ok().flatten().is_some() {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    })
                    .await
                    .is_ok()
                }
            };

            // Before the watch starts: pod (h) is gone from Kubernetes, and
            // old enough that its record should have a pod.
            let conversation_h = db::create_conversation(&pool).await.expect("create conversation h");
            let pod_h = create_pod(&pool, conversation_h.id, PodLimitOverrides::default()).await.expect("create_pod (h) should succeed");
            let terminal_h = db::create_sandbox_terminal(&pool, pod_h).await.expect("a terminal record");
            db::create_terminal_command(&pool, conversation_h.id, terminal_h.id, "stale-h", "sleep 300")
                .await
                .expect("a running command record");
            sqlx::query("UPDATE sandbox_pods SET created_at = now() - interval '10 minutes' WHERE id = $1")
                .bind(pod_h)
                .execute(&pool)
                .await
                .expect("age the pod past the reconciliation cut-off");
            pods_api(&client).delete(&pod_name(pod_h), &immediate_delete_params()).await.expect("delete pod (h) directly");
            assert!(wait_until_gone_from_kubernetes(pod_h).await, "pod (h) should leave Kubernetes");
            let conversation_young = db::create_conversation(&pool).await.expect("create conversation");
            let young = db::create_sandbox_pod(&pool, conversation_young.id).await.expect("a young pod record");
            let updated_before = updated_at(conversation_h.id).await;

            let watch = tokio::spawn(watch_pods(pool.clone()));

            assert!(wait_until_closed(pod_h).await, "the watch's first listing should close pod (h)'s record");
            let command_h = db::get_terminal_command(&pool, "stale-h").await.expect("get").expect("the command");
            assert_eq!(command_h.status, "lost", "its running command should be marked lost");
            assert!(
                db::list_sandbox_terminals_for_pod(&pool, pod_h).await.expect("terminals").is_empty(),
                "its terminal should be closed"
            );
            assert!(
                db::list_messages(&pool, conversation_h.id).await.expect("messages").is_empty(),
                "records are closed quietly, with no notice"
            );
            assert_eq!(updated_at(conversation_h.id).await, updated_before, "the conversation shouldn't move in the sidebar");

            // While the watch runs: pod (i) is deleted outside smelt, with no
            // connection open, so crash detection never sees it.
            let conversation_i = db::create_conversation(&pool).await.expect("create conversation i");
            let pod_i = create_pod(&pool, conversation_i.id, PodLimitOverrides::default()).await.expect("create_pod (i) should succeed");
            pods_api(&client).delete(&pod_name(pod_i), &immediate_delete_params()).await.expect("delete pod (i) directly");
            assert!(wait_until_closed(pod_i).await, "a pod deleted while the watch runs should have its record closed");
            assert!(
                db::list_messages(&pool, conversation_i.id).await.expect("messages").is_empty(),
                "closed quietly, with no notice"
            );

            assert!(db::sandbox_pod_is_live(&pool, young.id).await.expect("is live"), "a young record is left alone");
            watch.abort();
            db::terminate_sandbox_pod(&pool, young.id).await.expect("clean up the young record");

            // --- Exhausting reconnect attempts without Kubernetes ever
            // confirming death still cleans up *and* force-terminates the
            // pod — verified by create_pod succeeding again immediately
            // after, not staying blocked behind a stale live-pod row. A
            // hand-built pod with no agent in it (not `create_pod`, which
            // now always has one running the instant the pod is `Running`
            // — the agent is the image's own `ENTRYPOINT`, see Phase 2 of
            // SME-17) is what
            // makes every connect() attempt genuinely and deterministically
            // fail while the pod itself stays Running throughout, exactly
            // the "stuck reporting Running but unreachable" case this path
            // exists for. ---
            let conversation_g = db::create_conversation(&pool).await.expect("create conversation g");
            let pod_g_row = db::create_sandbox_pod(&pool, conversation_g.id).await.expect("create_sandbox_pod (g)");
            let pod_g = pod_g_row.id;
            // Explicit and small, on purpose — see
            // `sandbox_image_import::loader_pod_spec`'s comment: an
            // unspecified request/limit here would implicitly default to
            // `smelt-park`'s `LimitRange` `max` (64Gi/16 cores), which no
            // CI runner can schedule.
            let mut no_agent_limits = std::collections::BTreeMap::new();
            no_agent_limits.insert("cpu".to_string(), Quantity("250m".to_string()));
            no_agent_limits.insert("memory".to_string(), Quantity("128Mi".to_string()));
            let no_agent_pod = Pod {
                metadata: ObjectMeta {
                    name: Some(pod_name(pod_g)),
                    namespace: Some(NAMESPACE.to_string()),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    containers: vec![Container {
                        name: "sandbox".to_string(),
                        image: Some("busybox:1.36".to_string()),
                        command: Some(vec!["sleep".to_string(), "300".to_string()]),
                        resources: Some(ResourceRequirements { limits: Some(no_agent_limits), ..Default::default() }),
                        ..Default::default()
                    }],
                    restart_policy: Some("Never".to_string()),
                    ..Default::default()
                }),
                status: None,
            };
            pods_api(&client).create(&PostParams::default(), &no_agent_pod).await.expect("create no-agent pod (g)");
            wait_for_running(&pods_api(&client), &pod_name(pod_g)).await.expect("no-agent pod (g) should reach Running");

            let gave_up = reconnect_or_confirm_crash(&pool, pod_g).await;
            assert!(
                matches!(gave_up, Err(TerminalError::NoTerminal)),
                "should give up as NoTerminal once reconnect attempts are exhausted, got {:?}",
                gave_up.is_ok()
            );
            let pod_g_gone = pods_api(&client).get_opt(&pod_name(pod_g)).await.expect("get_opt");
            assert!(pod_g_gone.is_none(), "exhausting reconnect attempts should force-terminate the k8s pod");
            let recreated = create_pod(&pool, conversation_g.id, PodLimitOverrides::default()).await;
            assert!(
                recreated.is_ok(),
                "create_pod should succeed again immediately, not stay blocked behind a stale live-pod row, got {recreated:?}"
            );

            // --- An idle terminal (nothing running in it) still gets
            // covered by the pod-level message, even though it has no
            // per-command message of its own. ---
            let conversation_h = db::create_conversation(&pool).await.expect("create conversation h");
            let pod_h = create_pod(&pool, conversation_h.id, PodLimitOverrides::default()).await.expect("create_pod (h) should succeed");
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

            // --- Docker data PVC (SME-33): create_pod gives the pod the
            // conversation's own claim, so its images outlive the pod, and
            // teardown_conversation removes the claim with the pod. ---
            let conversation_j = db::create_conversation(&pool).await.expect("create conversation j");
            let pod_j = create_pod(&pool, conversation_j.id, PodLimitOverrides::default()).await.expect("create_pod (j) should succeed");
            let docker_pvcs = pvc_api(&client);
            let docker_claim = docker_pvc_name(conversation_j.id);
            assert!(
                docker_pvcs.get_opt(&docker_claim).await.expect("get_opt").is_some(),
                "create_pod should create the conversation's docker data claim {docker_claim}"
            );
            let spec_j = pods_api(&client).get(&pod_name(pod_j)).await.expect("get pod j").spec.expect("spec");
            let data_claim = spec_j
                .volumes
                .unwrap_or_default()
                .into_iter()
                .find(|v| v.name == DOCKER_DATA_VOLUME)
                .and_then(|v| v.persistent_volume_claim)
                .map(|c| c.claim_name);
            assert_eq!(data_claim.as_deref(), Some(docker_claim.as_str()), "pod j should mount its conversation's claim");
            // And /workspace is on a claim of its own (SME-32).
            let workspace_claim = workspace_pvc_name(conversation_j.id);
            assert!(
                docker_pvcs.get_opt(&workspace_claim).await.expect("get_opt").is_some(),
                "create_pod should create the conversation's workspace claim {workspace_claim}"
            );
            let spec_j = pods_api(&client).get(&pod_name(pod_j)).await.expect("get pod j").spec.expect("spec");
            let mounted_workspace = spec_j
                .volumes
                .unwrap_or_default()
                .into_iter()
                .find(|v| v.name == WORKSPACE_VOLUME)
                .and_then(|v| v.persistent_volume_claim)
                .map(|c| c.claim_name);
            assert_eq!(mounted_workspace.as_deref(), Some(workspace_claim.as_str()), "pod j's /workspace is its conversation's claim");

            teardown_conversation(&pool, conversation_j.id).await;
            // The claim's `pvc-protection` finalizer holds it until the pod
            // is really gone.
            let docker_claim_gone = tokio::time::timeout(Duration::from_secs(60), async {
                while docker_pvcs.get_opt(&docker_claim).await.ok().flatten().is_some() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
            .await;
            assert!(docker_claim_gone.is_ok(), "teardown_conversation should delete the docker data claim");
            let workspace_claim_gone = tokio::time::timeout(Duration::from_secs(60), async {
                while docker_pvcs.get_opt(&workspace_claim).await.ok().flatten().is_some() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
            .await;
            assert!(workspace_claim_gone.is_ok(), "teardown_conversation should delete the workspace claim");

            // --- Generic volumes: create_volume/delete_volume manage a
            // real PVC alongside the sandbox_volumes row, and a volume
            // mounted into a real pod is genuinely writable/readable —
            // see SME-17's
            // Phase 4. Folded in here rather than a separate test function
            // for the same MANAGER-singleton reason as everything else in
            // this test (see docs/testing.md). ---
            let volume_id = create_volume(&pool, "test-volume", "/data/testvol").await.expect("create_volume should succeed");
            let pvcs = pvc_api(&client);
            let pvc_name = sandbox_volume_pvc_name(volume_id);
            assert!(
                pvcs.get_opt(&pvc_name).await.expect("get_opt").is_some(),
                "create_volume should create the backing PVC"
            );

            let volumes = db::list_sandbox_volumes(&pool).await.expect("list_sandbox_volumes");
            let volume_session_id = unique_session_id(VOLUME_MOUNT_SESSION_LABEL);
            let volume_sandbox =
                get().create(&volume_session_id, "128Mi", "250m", &volumes).await.expect("create with a volume should succeed");
            let write =
                volume_sandbox.exec(&["sh", "-c", "echo hello > /data/testvol/marker.txt"]).await.expect("exec should succeed");
            assert_eq!(write.exit_code, 0, "writing into the mounted volume should succeed");
            let read = volume_sandbox.exec(&["cat", "/data/testvol/marker.txt"]).await.expect("exec should succeed");
            assert_eq!(read.stdout.trim(), "hello", "the mounted volume should be genuinely writable and readable");

            let volume_pods = pods_api(&client);
            volume_pods.delete(&volume_sandbox.pod_name, &immediate_delete_params()).await.ok();
            let volume_pod_name = volume_sandbox.pod_name.clone();
            std::mem::forget(volume_sandbox);
            // The PVC has Kubernetes' own `kubernetes.io/pvc-protection`
            // finalizer while a pod actively mounts it — the finalizer only
            // releases once the pod is genuinely gone, not just marked for
            // deletion, so this waits for that before deleting the volume.
            tokio::time::timeout(Duration::from_secs(30), async {
                while volume_pods.get_opt(&volume_pod_name).await.ok().flatten().is_some() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
            .await
            .expect("the volume-mounting pod should actually disappear, not just be marked for deletion");

            delete_volume(&pool, volume_id).await.expect("delete_volume should succeed");
            let pvc_gone = tokio::time::timeout(Duration::from_secs(15), async {
                while pvcs.get_opt(&pvc_name).await.ok().flatten().is_some() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
            .await;
            assert!(pvc_gone.is_ok(), "delete_volume should delete the backing PVC");
        })
        .await;

        // Best-effort cleanup regardless of pass/fail, matching this file's
        // existing convention (real-cluster tests, no automatic isolation).
        let pods = pods_api(&get().client);
        for n in 1..=20i64 {
            pods.delete(&pod_name(n), &immediate_delete_params())
                .await
                .ok();
        }
        // Each conversation's docker data claim (SME-33); ids are small here.
        let pvcs = pvc_api(&get().client);
        for n in 1..=30i64 {
            pvcs.delete(&docker_pvc_name(n), &DeleteParams::default()).await.ok();
        }

        outcome.expect(
            "terminal lifecycle integration test should complete within the timeout, not hang",
        );
    }

    /// Creates and sends a command in one step, waits for it to finish,
    /// returns its `command_id` — most of this test's steps are this exact
    /// shape, this just cuts the repetition.
    async fn run_and_wait(
        pool: &PgPool,
        conversation_id: i64,
        terminal_id: i64,
        command_id: &str,
        command: &str,
    ) -> String {
        db::create_terminal_command(pool, conversation_id, terminal_id, command_id, command)
            .await
            .expect("create_terminal_command");
        send_command(pool, terminal_id, command_id, command)
            .await
            .expect("send_command");
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

    /// Whether a `PodsChanged` arrives on `rx` within a few seconds,
    /// skipping any older ones still queued from earlier scenarios.
    async fn received_pods_changed(
        rx: &mut tokio::sync::broadcast::Receiver<events::AppEvent>,
    ) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(events::AppEvent::PodsChanged) => return true,
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    async fn any_message_contains(pool: &PgPool, conversation_id: i64, needle: &str) -> bool {
        db::list_messages(pool, conversation_id)
            .await
            .expect("list_messages")
            .iter()
            .any(|m| {
                m.blocks().ok().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|b| matches!(b, crate::anthropic::ContentBlock::Text { text } if text.contains(needle)))
                })
            })
    }

    #[tokio::test]
    async fn test_create_reaches_running_from_clean_slate() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("create");

        let sandbox = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("create should succeed");

        let pods = pods_api(&client);
        let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
        assert_eq!(pod.status.and_then(|s| s.phase).as_deref(), Some("Running"));

        pods.delete(&sandbox.pod_name, &immediate_delete_params())
            .await
            .ok();
        std::mem::forget(sandbox);
    }

    /// A pod that never reaches `Running` (real cause seen in this
    /// environment: `local-path`'s `WaitForFirstConsumer` provisioning
    /// stuck behind an unrelated pileup — see the precheck below) must not
    /// be left behind: nothing else in the system ever cleans up a pod
    /// `create` gave up waiting on, since `Sandbox::drop`'s cleanup queue
    /// only fires for a `Sandbox` that was actually returned. An orphaned
    /// pod here isn't just wasted — a caller that derives the same pod
    /// name deterministically (`sandbox_volume_pvc_name`'s pod-name
    /// counterpart) collides with it on every later attempt, exactly what
    /// happened to `test_terminal_lifecycle_end_to_end`'s volume-mount pod
    /// before this fix.
    ///
    /// Drives `create_with_running_timeout` directly with a 1ms timeout
    /// rather than the real (30s+) default, or mutating
    /// `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` — that env var is process-global
    /// and read by every other concurrently-running test in this suite, so
    /// overriding it here would risk timing them out too. 1ms guarantees
    /// the timeout fires before the first status check ever completes,
    /// independent of how fast this cluster actually schedules pods.
    #[tokio::test]
    async fn test_create_deletes_the_pod_it_just_created_if_it_never_reaches_running() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("timeout-cleanup");
        let name = format!("sandbox-{session_id}");

        let result = manager
            .create_with_running_timeout(
                &session_id,
                "128Mi",
                "250m",
                &DockerSidecar {
                    memory: "512Mi".to_string(),
                    cpu: "250m".to_string(),
                    storage: PodStorage::Ephemeral,
                },
                &[],
                Duration::from_millis(1),
            )
            .await;
        match result {
            Err(SandboxError::Timeout(_)) => {}
            Err(e) => panic!("expected a Timeout error, got a different error: {e}"),
            Ok(sandbox) => {
                std::mem::forget(sandbox);
                panic!("expected create to time out waiting for Running, but it succeeded");
            }
        }

        let pods = pods_api(&client);
        let gone = tokio::time::timeout(Duration::from_secs(15), async {
            while pods.get_opt(&name).await.ok().flatten().is_some() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        assert!(
            gone.is_ok(),
            "the pod create() gave up waiting on should have been cleaned up, not left orphaned"
        );
    }

    /// Every configured volume is mounted into every pod, but its claim can
    /// be missing from the pod's namespace — the cluster was rebuilt, the
    /// claim was deleted, or (as the browser tier found) the volume was
    /// created in another namespace. The pod must still start.
    #[tokio::test]
    async fn test_create_recreates_a_missing_volume_claim() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let pvcs = pvc_api(&client);
        let volume_id = 990_003;
        let claim = sandbox_volume_pvc_name(volume_id);
        pvcs.delete(&claim, &DeleteParams::default()).await.ok();
        let now = chrono::Utc::now().naive_utc();
        let volume = db::SandboxVolume {
            id: volume_id,
            name: "missing-claim".to_string(),
            mount_path: "/missing-claim".to_string(),
            created_at: now,
            updated_at: now,
        };

        let result = manager
            .create(&unique_session_id("missing-claim"), "128Mi", "500m", &[volume])
            .await;
        let claim_exists = matches!(pvcs.get_opt(&claim).await, Ok(Some(_)));
        if let Ok(sandbox) = &result {
            pods_api(&client)
                .delete(&sandbox.pod_name, &immediate_delete_params())
                .await
                .ok();
        }
        pvcs.delete(&claim, &DeleteParams::default()).await.ok();

        assert!(result.is_ok(), "the pod should start: {:?}", result.err().map(|e| e.to_string()));
        assert!(claim_exists, "the missing claim should have been recreated");
    }

    #[tokio::test]
    async fn test_create_applies_the_given_memory_and_cpu_limits() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("limits");

        let sandbox = manager
            .create(&session_id, "128Mi", "500m", &[])
            .await
            .expect("create should succeed");

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
        assert_eq!(limits.get("cpu"), Some(&Quantity("500m".to_string())));

        pods.delete(&sandbox.pod_name, &immediate_delete_params())
            .await
            .ok();
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

        let result = manager.create(&session_id, "128Gi", "250m", &[]).await;
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
        let sandbox = manager
            .create(&session_id, "64Mi", "250m", &[])
            .await
            .expect("create should succeed");
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
            .exec(
                &sandbox.pod_name,
                ["bash", "-c", "printf -v x '%*s' 200000000 ''; sleep 5"],
                &AttachParams::default(),
            )
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
        .await;

        // Delete the pod directly before asserting, not through the cleanup
        // queue: a Failed pod is never removed by Kubernetes, and every run
        // used to leave one behind.
        let name = sandbox.pod_name.clone();
        std::mem::forget(sandbox);
        pods.delete(&name, &immediate_delete_params()).await.ok();
        while pods.get_opt(&name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        let reason = reason.expect("pod should be confirmed dead within 30s of a real OOM trigger");
        assert_eq!(
            reason,
            Some("OOMKilled".to_string()),
            "a real OOM kill should surface as OOMKilled via the real Kubernetes API, not a constructed one"
        );
    }

    #[tokio::test]
    async fn test_create_reuses_existing_running_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("reuse");

        let first = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("first create should succeed");
        let second = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("second create should reuse, not error");

        assert_eq!(first.pod_name, second.pod_name);

        let pods = pods_api(&client);
        pods.delete(&first.pod_name, &immediate_delete_params())
            .await
            .ok();
        std::mem::forget(first);
        std::mem::forget(second);
    }

    #[tokio::test]
    async fn test_exec_captures_stdout_and_exit_code() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("exec");
        let sandbox = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("create should succeed");

        let result = sandbox
            .exec(&["echo", "hello"])
            .await
            .expect("exec should succeed");
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);

        let failing = sandbox
            .exec(&["bash", "-c", "exit 7"])
            .await
            .expect("exec should succeed even for nonzero exit");
        assert_eq!(failing.exit_code, 7);

        let pods = pods_api(&client);
        pods.delete(&sandbox.pod_name, &immediate_delete_params())
            .await
            .ok();
        std::mem::forget(sandbox);
    }

    /// New observable behavior from
    /// SME-17's Phase 3 —
    /// commands run as the pre-created, unprivileged `sandbox` user by
    /// default (not root, unlike the old `debian:trixie-slim` image with
    /// no `USER` set), with passwordless `sudo` still available for
    /// anything that genuinely needs root.
    #[tokio::test]
    async fn test_sandbox_commands_run_as_non_root_with_sudo_available() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("non-root");
        let sandbox = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("create should succeed");

        let whoami = sandbox
            .exec(&["whoami"])
            .await
            .expect("exec should succeed");
        assert_eq!(
            whoami.stdout.trim(),
            "sandbox",
            "commands should run as the pre-created `sandbox` user, not root"
        );

        let sudo_whoami = sandbox
            .exec(&["sudo", "whoami"])
            .await
            .expect("exec should succeed");
        assert_eq!(
            sudo_whoami.stdout.trim(),
            "root",
            "sudo should still reach root for a command that genuinely needs it"
        );

        // The everyday tools are already there: a first coding task used
        // to fail with `python3: command not found` and spend a minute
        // installing it (SME-41 D10).
        for tool in ["python3", "git", "curl"] {
            let found = sandbox
                .exec(&["sh", "-c", &format!("command -v {tool}")])
                .await
                .expect("exec should succeed");
            assert!(!found.stdout.trim().is_empty(), "{tool} should be installed in the sandbox image");
        }

        let pods = pods_api(&client);
        pods.delete(&sandbox.pod_name, &immediate_delete_params())
            .await
            .ok();
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
        let sandbox = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("create should succeed");
        let pods = pods_api(&client);

        assert_eq!(
            pod_death_reason(&pods, &sandbox.pod_name).await,
            None,
            "a genuinely Running pod is inconclusive"
        );

        pods.delete(&sandbox.pod_name, &immediate_delete_params())
            .await
            .expect("delete should succeed");
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
        let sandbox = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("create should succeed");
        let pod_name = sandbox.pod_name.clone();

        manager
            .delete(sandbox)
            .await
            .expect("delete should succeed");

        let pods = pods_api(&client);
        let still_there = pods
            .get_opt(&pod_name)
            .await
            .expect("get_opt should not error");
        assert!(
            still_there.is_none(),
            "pod should be gone immediately after manager.delete returns Ok"
        );
    }

    #[tokio::test]
    async fn test_dropping_without_delete_still_cleans_up_via_drain_task() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("drop");
        let sandbox = manager
            .create(&session_id, "128Mi", "250m", &[])
            .await
            .expect("create should succeed");
        let pod_name = sandbox.pod_name.clone();

        drop(sandbox); // no manager.delete call — this is the path under test

        let pods = pods_api(&client);
        let gone = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if pods
                    .get_opt(&pod_name)
                    .await
                    .expect("get_opt should not error")
                    .is_none()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        assert!(
            gone.is_ok(),
            "drain task should have deleted the pod within the timeout"
        );
    }
}

