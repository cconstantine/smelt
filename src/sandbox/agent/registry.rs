use super::*;

/// The per-pod registry — one WebSocket connection per pod, shared by
/// every terminal that pod hosts (one agent *process* per pod — see
/// SME-9's "Why N pods and N terminals"). Also what smelt knows about pods
/// it must not connect to: ones torn down, and ones whose agent is
/// outdated. Kept together under one lock, so a connection finishing
/// while its pod is torn down is either closed by the teardown or refused
/// by `register`, never kept.
#[derive(Default)]
pub(in crate::sandbox) struct Registry {
    pub(in crate::sandbox) connections: HashMap<i64, Arc<TerminalConnection>>,
    /// Pod ids never come back, so this is never cleared.
    pub(in crate::sandbox) torn_down: std::collections::HashSet<i64>,
    /// Pods whose agent speaks another major (or none), and the image each
    /// runs. The image can't change under a pod, so it doesn't expire.
    pub(in crate::sandbox) outdated: HashMap<i64, Outdated>,
}

/// What smelt learned from a pod whose agent it refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::sandbox) struct Outdated {
    /// From its hello; `None` for an agent from before versioning.
    pub(in crate::sandbox) found: Option<ProtocolVersion>,
    /// The image its sandbox container runs, if it could be read; when it
    /// couldn't, the next connect reads it again (review 1).
    pub(in crate::sandbox) image: Option<String>,
}

impl Outdated {
    /// The error a caller gets, its advice decided by comparing the pod's
    /// image with the one a new pod gets now.
    pub(in crate::sandbox) fn error(&self) -> TerminalError {
        TerminalError::AgentOutdated {
            found: self.found,
            image: judge_image(self.image.as_deref(), &default_sandbox_image()),
        }
    }
}

/// How a refused pod's `image` compares with `new`, the image a new pod
/// gets, by their full names: Kubernetes keeps a pod's image as written.
pub(in crate::sandbox) fn judge_image(image: Option<&str>, new: &str) -> PodImage {
    match image {
        Some(image) if full_image_name(image) == full_image_name(new) => PodImage::SameAsNew(image.to_string()),
        Some(_) => PodImage::Older,
        None => PodImage::Unknown,
    }
}

pub(in crate::sandbox) static REGISTRY: LazyLock<StdMutex<Registry>> = LazyLock::new(Default::default);

pub(in crate::sandbox) fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

pub(in crate::sandbox) fn registry_get(pod_id: i64) -> Option<Arc<TerminalConnection>> {
    registry().connections.get(&pod_id).cloned()
}

pub(in crate::sandbox) fn registry_contains(pod_id: i64) -> bool {
    registry().connections.contains_key(&pod_id)
}

/// Why `register` refused a connection.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::sandbox) enum Refused {
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
pub(in crate::sandbox) fn register(pod_id: i64, conn: Arc<TerminalConnection>) -> Result<(), Refused> {
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
pub(in crate::sandbox) fn deregister(pod_id: i64) {
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
pub(in crate::sandbox) fn forget_connection(pod_id: i64) {
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
pub(in crate::sandbox) fn deregister_if_current(pod_id: i64, conn: &Arc<TerminalConnection>) -> bool {
    let mut registry = registry();
    match registry.connections.get(&pod_id) {
        Some(current) if Arc::ptr_eq(current, conn) => {
            registry.connections.remove(&pod_id);
            true
        }
        _ => false,
    }
}

/// What smelt knows of the pod's agent, if it's known to be outdated.
pub(in crate::sandbox) fn outdated(pod_id: i64) -> Option<Outdated> {
    registry().outdated.get(&pod_id).cloned()
}

/// How an agent on `agent` compares with a smelt on `smelt` of the same
/// major.
pub(in crate::sandbox) fn classify_agent(agent: ProtocolVersion, smelt: ProtocolVersion) -> crate::api::pods::AgentStatus {
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
    if let Some(outdated) = registry.outdated.get(&pod_id) {
        return Some(crate::api::pods::AgentStatus::RestartRequired {
            version: outdated.found.map(|version| version.to_string()),
            rebuild_image: matches!(outdated.error(), TerminalError::AgentOutdated { image: PodImage::SameAsNew(_), .. }),
        });
    }
    let conn = registry.connections.get(&pod_id)?;
    Some(classify_agent(conn.agent_version, PROTOCOL_VERSION))
}
