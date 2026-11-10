//! Reaching a pod's agent: the dialer traits, the cluster dialer, and image names.

use super::*;

/// How long a request to the agent waits for its reply.
pub(in crate::sandbox) const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// A byte stream to a pod's agent.
pub(in crate::sandbox) trait AgentIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AgentIo for T {}

pub(in crate::sandbox) type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// How smelt reaches a pod's agent, and what it asks the cluster about the
/// pod along the way. `ClusterDialer` is the real one; tests put a fake
/// agent on a loopback port behind `dialer_for` instead.
pub(in crate::sandbox) trait AgentDialer: Send + Sync {
    fn dial(&self, pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>>;
    /// Whether the pod is `Running`: a first connection waits for that.
    fn is_running(&self, pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>>;
    /// `Some(reason)` once the cluster says the pod is dead, `None` while it
    /// may still be alive. See `pod_death_reason`.
    fn death_reason(&self, pod_id: i64) -> BoxFuture<'_, Option<Option<String>>>;
    /// The image the pod's sandbox container runs, `None` if that can't be
    /// read (SME-121).
    fn image(&self, pod_id: i64) -> BoxFuture<'_, Option<String>>;
}

/// A port-forward to the agent's port in the pod. Another database's pod
/// of the same name reads as absent and is never dialed (SME-115).
pub(in crate::sandbox) struct ClusterDialer(pub(in crate::sandbox) kube::Client, pub(in crate::sandbox) db::SmeltInstance);

impl AgentDialer for ClusterDialer {
    fn dial(&self, pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>> {
        Box::pin(async move {
            let name = pod_name(pod_id);
            refuse_foreign(&pods_api(&self.0), &name, &self.1).await?;
            let mut forward = pods_api(&self.0).portforward(&name, &[AGENT_PORT]).await?;
            let stream = require_stream(forward.take_stream(AGENT_PORT), "the agent port's port-forward")?;
            Ok(Box::new(stream) as Box<dyn AgentIo>)
        })
    }

    fn is_running(&self, pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>> {
        Box::pin(async move {
            let pod = read_our_pod(&pods_api(&self.0), &pod_name(pod_id), &self.1).await?;
            Ok(pod.and_then(|p| p.status).and_then(|s| s.phase).as_deref() == Some("Running"))
        })
    }

    fn death_reason(&self, pod_id: i64) -> BoxFuture<'_, Option<Option<String>>> {
        Box::pin(async move { pod_death_reason(&pods_api(&self.0), &pod_name(pod_id), &self.1).await })
    }

    fn image(&self, pod_id: i64) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let pod = read_our_pod(&pods_api(&self.0), &pod_name(pod_id), &self.1).await.ok()??;
            sandbox_container_image(&pod)
        })
    }
}

/// `reference` the way containerd names it: Docker Hub's `docker.io` and
/// `library/` filled in, and `:latest` when it has neither tag nor digest
/// (`smelt-sandbox` is `docker.io/library/smelt-sandbox:latest`).
pub(in crate::sandbox) fn full_image_name(reference: &str) -> String {
    let reference = reference.trim();
    let (name, digest) = match reference.split_once('@') {
        Some((name, digest)) => (name, Some(digest)),
        None => (reference, None),
    };
    // `index.docker.io` is Docker Hub's old name for `docker.io`.
    let renamed = name.strip_prefix("index.docker.io/").map(|path| format!("docker.io/{path}"));
    let name = renamed.as_deref().unwrap_or(name);
    // Docker's reference parser takes a first part with a dot, a port,
    // capitals, or `localhost` as a registry.
    let first = name.split('/').next().unwrap_or_default();
    let has_registry = name.contains('/')
        && (first.contains('.') || first.contains(':') || first == "localhost" || first.chars().any(|c| c.is_ascii_uppercase()));
    let mut full = if has_registry {
        match name.strip_prefix("docker.io/") {
            Some(path) if !path.contains('/') => format!("docker.io/library/{path}"),
            _ => name.to_string(),
        }
    } else if name.contains('/') {
        format!("docker.io/{name}")
    } else {
        format!("docker.io/library/{name}")
    };
    let last = full.rsplit('/').next().unwrap_or_default();
    match digest {
        Some(digest) => full = format!("{full}@{digest}"),
        None if !last.contains(':') => full.push_str(":latest"),
        None => {}
    }
    full
}

/// The image of `pod`'s `sandbox` container.
pub(in crate::sandbox) fn sandbox_container_image(pod: &Pod) -> Option<String> {
    let spec = pod.spec.as_ref()?;
    spec.containers.iter().find(|c| c.name == "sandbox")?.image.clone()
}

/// The dialer for `pod_id`: the cluster, unless a test put a fake agent in
/// for that pod.
#[cfg_attr(not(test), allow(unused_variables))] // only a test puts a fake in
pub(in crate::sandbox) fn dialer_for(pod_id: i64, instance: db::SmeltInstance) -> Result<Arc<dyn AgentDialer>, SandboxError> {
    #[cfg(test)]
    if let Some(dialer) = test_dialers().lock().unwrap_or_else(|e| e.into_inner()).get(&pod_id) {
        return Ok(dialer.clone());
    }
    Ok(Arc::new(ClusterDialer(get()?.client.clone(), instance)))
}

/// Fake agents, by the pod id each test owns. Keyed by pod so tests running
/// in parallel never see each other's.
#[cfg(test)]
pub(in crate::sandbox) fn test_dialers() -> &'static StdMutex<HashMap<i64, Arc<dyn AgentDialer>>> {
    static DIALERS: LazyLock<StdMutex<HashMap<i64, Arc<dyn AgentDialer>>>> = LazyLock::new(Default::default);
    &DIALERS
}
