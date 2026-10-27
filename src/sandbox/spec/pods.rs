//! The pod spec itself: its containers, volumes and delete options.

use super::*;

/// The Kubernetes pod name of smelt's pod `pod_id`.
pub fn pod_name(pod_id: i64) -> String {
    format!("sandbox-{pod_id}")
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
pub(in crate::sandbox) const POD_DELETE_GRACE_SECS: u32 = 20;

/// Every delete of a sandbox pod smelt makes.
pub(crate) fn pod_delete_params() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(POD_DELETE_GRACE_SECS),
        ..Default::default()
    }
}

/// `pod_delete_params` held to the pod a `Sandbox` made or reused, by its
/// `uid`: a pod that has taken its name since is refused (409) and left
/// (SME-180 review 1).
pub(in crate::sandbox) fn sandbox_delete_params(uid: Option<&str>) -> DeleteParams {
    DeleteParams {
        preconditions: Some(kube::api::Preconditions { uid: uid.map(str::to_string), resource_version: None }),
        ..pod_delete_params()
    }
}

/// Tests' own cleanup, which needs nothing to stop cleanly.
#[cfg(test)]
pub(in crate::sandbox) fn immediate_delete_params() -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    }
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

/// Starts dockerd inside the sidecar's own cgroup instead of through the
/// docker:dind image's entrypoint; see the script's own comment.
pub(in crate::sandbox) const START_DOCKERD_SCRIPT: &str = include_str!("../../../docker/sandbox/start-dockerd.sh");
/// `agent_volumes` is every volume this conversation mounts — its own and the
/// ones passed down to it (SME-239); an empty slice is every pod today's shape.

/// `memory` is a plain Kubernetes `Quantity` string (`"8Gi"`)
/// — no app-side parsing or validation of the format; an invalid value is
/// rejected by the Kubernetes API itself when the pod is actually
/// created, surfacing back through `SandboxError::Kube` as an ordinary
/// error. Bounded from above by the `smelt-park` namespace's own
/// `LimitRange` (`k8s/smelt-park-rbac.yaml`), not by anything here.
pub(in crate::sandbox) fn build_pod_spec(
    name: &str,
    memory: &str,
    docker: &DockerSidecar,
    volumes: &[db::SandboxVolume],
    agent_volumes: &[db::AgentVolume],
    instance: &str,
) -> Pod {
    let (mut pod_volumes, mut user_mounts) = volume_mounts_for(volumes);
    // A model volume goes on both containers, exactly as a global one does:
    // a bind mount dockerd resolves finds what the model wrote either way
    // (SME-239).
    let (agent_vols, agent_mounts) = agent_volume_mounts_for(agent_volumes);
    pod_volumes.extend(agent_vols);
    user_mounts.extend(agent_mounts);
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
                // Fetched the same way as the sandbox image — see
                // `default_image_pull_policy`, which explains why the
                // default is `Never`.
                image_pull_policy: Some(default_image_pull_policy().to_string()),
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
                // The default `Never`, not Kubernetes' own (`IfNotPresent`
                // for a tag like `src-<hash>`, `Always` for `:latest`): the
                // image scripts/build-sandbox-image.sh delivers went straight
                // into the node's local image store (`ctr images import`)
                // with no registry involved at all — a real pull would just
                // fail (there's no such image on any real registry to pull),
                // which is exactly what happened the first time this was left
                // unset. A deployment whose images are in a registry says so
                // with SANDBOX_IMAGE_PULL_POLICY.
                image_pull_policy: Some(default_image_pull_policy().to_string()),
                // No `command` override — the image's own `ENTRYPOINT` is
                // the sandbox agent, so it's already running (and keeping
                // the pod alive) the moment the container starts. See
                // SME-17.
                resources: Some(memory_only(memory)),
                volume_mounts: Some(shared_mounts),
                ..Default::default()
            }],
            volumes: Some(pod_volumes),
            // Only set when SANDBOX_IMAGE_PULL_SECRET names secrets: an
            // image that never came from a registry needs no credentials, and
            // gets none. Both containers fetch theirs the same way, so one
            // pod-level setting covers the pair, as SANDBOX_IMAGE_PULL_POLICY
            // does.
            image_pull_secrets: default_image_pull_secrets(),
            // Every image the system runs — server, sandbox agent, dind
            // sidecar, and the language-server pod images — is built for
            // linux/amd64 only (`publish-images.yml` builds natively on
            // ubuntu-latest; nothing in the project cross-compiles), and the
            // nodes it runs on are all x86_64. So a pod must never reach a
            // node of another architecture: an amd64-only image has no
            // arm64 manifest, and the pod would sit `Pending` on a platform
            // mismatch instead of starting. `kubernetes.io/arch` is a label
            // every kubelet sets itself, so this is a hard pin, not a
            // preference. Language-server pods need no matching pin: they're
            // bound to their sandbox pod's node by `nodeName`
            // (`lsp::pods::server_pod_spec`), and so land wherever the
            // sandbox pod did.
            node_selector: Some(std::collections::BTreeMap::from([(
                "kubernetes.io/arch".to_string(),
                "amd64".to_string(),
            )])),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    }
}

/// dockerd's arguments after `START_DOCKERD_SCRIPT` — a unix socket only,
/// never TCP: the pod shares one network namespace, so a TCP listener
/// would be reachable through `open_pod_target` and a preview link.
pub(in crate::sandbox) fn dockerd_args() -> Vec<String> {
    vec![
        format!("--host={DOCKER_HOST}"),
        format!("--group={DOCKER_GID}"),
        format!("--bip={DOCKER_BRIDGE_IP}"),
        "--default-address-pool".to_string(),
        format!("base={DOCKER_NETWORK_POOL},size=24"),
    ]
}
