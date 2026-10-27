//! Memory limits: the defaults, and a container's `ResourceRequirements`.

use super::*;

/// `SANDBOX_MEMORY_LIMIT`, default `"8Gi"` if unset or empty. The
/// *default* a pod gets when `create_pod`'s caller doesn't specify its own
/// `memory_limit` — see SME-12's "Per-pod limit overrides."
pub(in crate::sandbox) fn default_memory_limit() -> String {
    std::env::var("SANDBOX_MEMORY_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8Gi".to_string())
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
pub(in crate::sandbox) fn running_wait_timeout() -> Duration {
    let secs = std::env::var("SANDBOX_RUNNING_WAIT_TIMEOUT_SECS")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_RUNNING_WAIT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// `SANDBOX_DOCKER_MEMORY_LIMIT`, default `"8Gi"` — see
/// `default_memory_limit`. Nested containers count against it, not the
/// sandbox container's limit.
pub(in crate::sandbox) fn default_docker_memory_limit() -> String {
    std::env::var("SANDBOX_DOCKER_MEMORY_LIMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "8Gi".to_string())
}

/// A container's resources (SME-77): a memory limit, an explicit zero
/// memory request and nothing for CPU. Without the request Kubernetes
/// copies the limit into it, so every idle pod reserved its whole limit
/// on the node; with no CPU limit or request, pods share the node's CPU.
pub(in crate::sandbox) fn memory_only(memory: &str) -> ResourceRequirements {
    let quantity = |q: &str| std::collections::BTreeMap::from([("memory".to_string(), Quantity(q.to_string()))]);
    ResourceRequirements {
        limits: Some(quantity(memory)),
        requests: Some(quantity("0")),
        ..Default::default()
    }
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
    pub(in crate::sandbox) fn resolve(self, conversation_id: i64) -> (String, DockerSidecar) {
        let docker = DockerSidecar {
            memory: self.docker_memory.unwrap_or_else(default_docker_memory_limit),
            storage: PodStorage::Conversation(conversation_id),
        };
        (self.memory.unwrap_or_else(default_memory_limit), docker)
    }
}

/// Every running container's memory and CPU limits in `pod`: its
/// containers and its native sidecars (init containers that keep running).
pub(in crate::sandbox) fn pod_container_limits(pod: &Pod) -> (Vec<String>, Vec<String>) {
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
