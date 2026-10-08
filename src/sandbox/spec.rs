//! Pod and claim specs: names, defaults, mounts, limits and the pod spec
//! itself. Pure: nothing here talks to the cluster.

use super::*;

/// The Kubernetes pod name of smelt's pod `pod_id`.
pub fn pod_name(pod_id: i64) -> String {
    format!("sandbox-{pod_id}")
}

/// The sandbox user's home directory — fixed by Phase 1's `useradd -m
/// sandbox` in `docker/sandbox/Dockerfile`, not derived from anything at
/// runtime.
pub(super) const SANDBOX_HOME: &str = "/home/sandbox";

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
pub(super) fn validate_mount_path(path: &str) -> Result<(), String> {
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

/// Why `pod` isn't running yet, from its first failing condition — e.g.
/// "PodScheduled: Unschedulable: 0/1 nodes are available: persistentvolumeclaim
/// … not found." Reported when waiting times out, so the error says why.
pub(super) fn pod_pending_detail(pod: &Pod) -> Option<String> {
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
pub(super) const FATAL_WAITING_REASONS: [&str; 7] = [
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
pub(super) fn pod_startup_failure(pod: &Pod) -> Option<String> {
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

pub(super) fn resolve_mount_path(path: &str) -> String {
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
pub(super) fn sandbox_volume_pvc_name(volume_id: i64) -> String {
    format!("sandbox-volume-{volume_id}")
}

/// Every configured volume's `Volume`/`VolumeMount` pair for a pod spec —
/// pulled out as a pure function over an already-fetched list, same
/// testability reason as `check_pod_guard`/`resolve_pod_id`. Every pod
/// gets every volume, unconditionally — see the idea's "not something
/// chosen per conversation."
pub(super) fn volume_mounts_for(volumes: &[db::SandboxVolume]) -> (Vec<Volume>, Vec<VolumeMount>) {
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
pub(super) const POD_DELETE_GRACE_SECS: u32 = 20;

/// Every delete of a sandbox pod smelt makes.
pub(crate) fn pod_delete_params() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(POD_DELETE_GRACE_SECS),
        ..Default::default()
    }
}

/// Tests' own cleanup, which needs nothing to stop cleanly.
#[cfg(test)]
pub(super) fn immediate_delete_params() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    }
}

/// `SANDBOX_MEMORY_LIMIT`, default `"8Gi"` if unset or empty. The
/// *default* a pod gets when `create_pod`'s caller doesn't specify its own
/// `memory_limit` — see SME-12's "Per-pod limit overrides."
pub(super) fn default_memory_limit() -> String {
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
/// confirmed by spike, not assumed. `:latest` is what a server with no
/// setting (the dev server) runs; `scripts/check.sh`, `browser-tier`,
/// `check-server` and CI set `SANDBOX_IMAGE` to the image named after the
/// working tree's agent sources (`scripts/sandbox-image-ref`, SME-102), and
/// tests that build their own pod specs read it here too.
pub(crate) fn default_sandbox_image() -> String {
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
pub(super) fn running_wait_timeout() -> Duration {
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
pub(super) fn default_docker_memory_limit() -> String {
    std::env::var("SANDBOX_DOCKER_MEMORY_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8Gi".to_string())
}

/// Shared by the sandbox and Docker sidecar containers: projects that
/// bind-mount host paths go here, and terminals start here.
pub const WORKSPACE_DIR: &str = "/workspace";
pub(super) const WORKSPACE_VOLUME: &str = "workspace";
/// Holds only dockerd's socket, never dockerd's own `/run/docker`: that
/// holds containerd's state and pid file, and a copy shared with the
/// sandbox survives a sidecar restart and makes the next dockerd fail
/// (SME-33's spike).
pub(super) const DOCKER_SOCK_DIR: &str = "/run/docker-sock";
pub(super) const DOCKER_SOCK_VOLUME: &str = "docker-sock";
pub(super) const DOCKER_HOST: &str = "unix:///run/docker-sock/docker.sock";
pub(super) const DOCKER_DATA_VOLUME: &str = "docker-data";
/// The `docker` group's GID in the sandbox image
/// (docker/sandbox/Dockerfile); dockerd gives it the socket.
pub(super) const DOCKER_GID: u32 = 2375;
/// The sandbox user's uid:gid, pinned in the image
/// (docker/sandbox/Dockerfile); the Docker sidecar gives it /workspace.
pub(super) const SANDBOX_OWNER: &str = "1000:1000";

/// The conversation's Docker data PVC, `sandbox-docker-<id>`.
pub(super) fn docker_pvc_name(conversation_id: i64) -> String {
    format!("sandbox-docker-{conversation_id}")
}

/// The conversation's /workspace PVC, `sandbox-workspace-<id>` (SME-32).
pub(crate) fn workspace_pvc_name(conversation_id: i64) -> String {
    format!("sandbox-workspace-{conversation_id}")
}

/// Label naming a conversation, on its Docker data PVC and on every pod
/// that mounts that PVC.
pub(super) const CONVERSATION_LABEL: &str = "smelt/conversation";

/// Label naming the database an object belongs to: its `smelt_instance`
/// id (SME-115). Every smelt server shares the namespace and names its
/// objects after its own database's ids, so this is what tells one
/// server's `sandbox-workspace-7` from another's.
pub(crate) const INSTANCE_LABEL: &str = "smelt/instance";

/// The instance tests' own ephemeral pods carry: they have no database.
#[cfg(test)]
pub(crate) const TEST_INSTANCE: &str = "smelt-tests";

/// The instance of the tests' own pods, as a database's would read.
#[cfg(test)]
pub(crate) fn test_instance() -> db::SmeltInstance {
    db::SmeltInstance { id: TEST_INSTANCE.to_string(), owns_unlabelled: false }
}

/// Whose a cluster object is, from one server's side (SME-115).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// Labelled with this server's instance.
    Ours,
    /// No instance label: made before SME-115. Only adoption touches it.
    Unlabelled,
    /// Another database's.
    Foreign,
}

/// Whose `meta`'s object is, for a server of `instance`. Only `Ours` may
/// be deleted, reused or mounted.
pub fn ownership(meta: &ObjectMeta, instance: &str) -> Ownership {
    match meta.labels.as_ref().and_then(|labels| labels.get(INSTANCE_LABEL)) {
        None => Ownership::Unlabelled,
        // An empty instance (never read) is nobody's.
        Some(label) if !instance.is_empty() && label == instance => Ownership::Ours,
        Some(_) => Ownership::Foreign,
    }
}

/// Whether a pod counts as this database's for reading it and for keeping
/// its record open: ours, or one from before SME-115 while this database
/// owns those (adoption missed it: it may still be ours). Another
/// database's pod reads as absent.
pub fn counts_as_ours(meta: &ObjectMeta, instance: &db::SmeltInstance) -> bool {
    match ownership(meta, &instance.id) {
        Ownership::Ours => true,
        Ownership::Unlabelled => instance.owns_unlabelled,
        Ownership::Foreign => false,
    }
}

/// The labels every object a server of `instance` creates carries.
pub(super) fn instance_labels(instance: &str) -> std::collections::BTreeMap<String, String> {
    [(INSTANCE_LABEL.to_string(), instance.to_string())].into()
}

/// `SANDBOX_DOCKER_STORAGE_SIZE`, default `"20Gi"` — see
/// `default_memory_limit`. Each conversation's Docker data PVC requests
/// this much.
pub(super) fn default_docker_storage_size() -> String {
    std::env::var("SANDBOX_DOCKER_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "20Gi".to_string())
}

/// `SANDBOX_WORKSPACE_STORAGE_SIZE`, default `"20Gi"` — see
/// `default_memory_limit`. Each conversation's /workspace PVC requests
/// this much.
pub(super) fn default_workspace_storage_size() -> String {
    std::env::var("SANDBOX_WORKSPACE_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "20Gi".to_string())
}

/// A conversation's claims: its Docker data and its /workspace (SME-32),
/// both kept across its pods and deleted with it. Each comes with its
/// name, so a caller needn't read it back out of the spec's `Option`.
pub(super) fn conversation_pvc_specs(conversation_id: i64, instance: &str) -> [(String, PersistentVolumeClaim); 2] {
    [
        (docker_pvc_name(conversation_id), default_docker_storage_size()),
        (workspace_pvc_name(conversation_id), default_workspace_storage_size()),
    ]
    .map(|(name, size)| {
        let spec = build_conversation_pvc_spec(name.clone(), conversation_id, size, instance);
        (name, spec)
    })
}

pub(super) fn build_conversation_pvc_spec(
    name: String,
    conversation_id: i64,
    size: String,
    instance: &str,
) -> PersistentVolumeClaim {
    let mut requests = std::collections::BTreeMap::new();
    requests.insert("storage".to_string(), Quantity(size));
    let mut labels = instance_labels(instance);
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

/// `SANDBOX_DOCKER_IMAGE`, default `"docker.io/library/docker:29-dind"` —
/// see `default_sandbox_image`. Delivered into the node like the sandbox
/// image, and only its `dockerd` is used: never its entrypoint.
pub(super) fn default_docker_image() -> String {
    std::env::var("SANDBOX_DOCKER_IMAGE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "docker.io/library/docker:29-dind".to_string())
}

/// Starts dockerd inside the sidecar's own cgroup instead of through the
/// docker:dind image's entrypoint; see the script's own comment.
pub(super) const START_DOCKERD_SCRIPT: &str = include_str!("../../docker/sandbox/start-dockerd.sh");

/// `memory` is a plain Kubernetes `Quantity` string (`"8Gi"`)
/// — no app-side parsing or validation of the format; an invalid value is
/// rejected by the Kubernetes API itself when the pod is actually
/// created, surfacing back through `SandboxError::Kube` as an ordinary
/// error. Bounded from above by the `smelt-park` namespace's own
/// `LimitRange` (`k8s/smelt-park-rbac.yaml`), not by anything here.
pub(super) fn build_pod_spec(
    name: &str,
    memory: &str,
    docker: &DockerSidecar,
    volumes: &[db::SandboxVolume],
    instance: &str,
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
    let mut labels = instance_labels(instance);
    match docker.storage {
        PodStorage::Conversation(conversation_id) => {
            labels.insert(CONVERSATION_LABEL.to_string(), conversation_id.to_string());
        }
        #[cfg(test)]
        PodStorage::Ephemeral => {}
    }

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(NAMESPACE.to_string()),
            labels: Some(labels),
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
pub(super) fn memory_only(memory: &str) -> ResourceRequirements {
    let quantity = |q: &str| std::collections::BTreeMap::from([("memory".to_string(), Quantity(q.to_string()))]);
    ResourceRequirements {
        limits: Some(quantity(memory)),
        requests: Some(quantity("0")),
        ..Default::default()
    }
}

pub(super) fn mount(volume: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: volume.to_string(),
        mount_path: path.to_string(),
        ..Default::default()
    }
}

pub(super) fn empty_dir_volume(name: &str) -> Volume {
    Volume {
        name: name.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    }
}

/// /workspace: the conversation's own claim, so a new pod finds it as the
/// last one left it (SME-32).
pub(super) fn workspace_volume(storage: &PodStorage) -> Volume {
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

pub(super) fn docker_data_volume(storage: &PodStorage) -> Volume {
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
pub(super) fn dockerd_args() -> Vec<String> {
    vec![
        format!("--host={DOCKER_HOST}"),
        format!("--group={DOCKER_GID}"),
        format!("--bip={DOCKER_BRIDGE_IP}"),
        "--default-address-pool".to_string(),
        format!("base={DOCKER_NETWORK_POOL},size=24"),
    ]
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
    pub(super) fn resolve(self, conversation_id: i64) -> (String, DockerSidecar) {
        let docker = DockerSidecar {
            memory: self.docker_memory.unwrap_or_else(default_docker_memory_limit),
            storage: PodStorage::Conversation(conversation_id),
        };
        (self.memory.unwrap_or_else(default_memory_limit), docker)
    }
}

/// Every running container's memory and CPU limits in `pod`: its
/// containers and its native sidecars (init containers that keep running).
pub(super) fn pod_container_limits(pod: &Pod) -> (Vec<String>, Vec<String>) {
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

// --- Generic volumes ---

/// `SANDBOX_VOLUME_STORAGE_SIZE`, default `"10Gi"` — same pattern as
/// `default_memory_limit`. Every generic volume's PVC requests this much
/// capacity; not currently configurable per volume (nothing in
/// SME-17 calls for that), just
/// a documented fixed default.
pub(super) fn default_volume_storage_size() -> String {
    std::env::var("SANDBOX_VOLUME_STORAGE_SIZE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "10Gi".to_string())
}

pub(super) fn build_volume_pvc_spec(volume_id: i64, instance: &str) -> PersistentVolumeClaim {
    let mut requests = std::collections::BTreeMap::new();
    requests.insert(
        "storage".to_string(),
        Quantity(default_volume_storage_size()),
    );

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(sandbox_volume_pvc_name(volume_id)),
            namespace: Some(NAMESPACE.to_string()),
            labels: Some(instance_labels(instance)),
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
