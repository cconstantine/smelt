//! Sandbox lifecycle: a disposable Kubernetes Pod per coding session, in the
//! `smelt-park` namespace. See docs/projects/plans/k8s-sandbox.md.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Container, Pod, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, AttachParams, DeleteParams, PostParams};
use tokio::sync::mpsc;

// No LimitRange in the smelt-park namespace (see plan) — these are the
// only enforcement there is.
const CPU_LIMIT: &str = "500m";
const MEMORY_LIMIT: &str = "512Mi";

const NAMESPACE: &str = "smelt-park";
const RUNNING_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

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

fn pod_name(session_id: &str) -> String {
    format!("sandbox-{session_id}")
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
        use tokio::io::AsyncReadExt;

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

    pub async fn create(&self, session_id: &str) -> Result<Sandbox, SandboxError> {
        let pods = pods_api(&self.client);
        let name = pod_name(session_id);

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
                pods.create(&PostParams::default(), &build_pod_spec(&name)).await?;
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

fn build_pod_spec(name: &str) -> Pod {
    let mut limits = BTreeMap::new();
    limits.insert("cpu".to_string(), Quantity(CPU_LIMIT.to_string()));
    limits.insert("memory".to_string(), Quantity(MEMORY_LIMIT.to_string()));

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(NAMESPACE.to_string()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "sandbox".to_string(),
                image: Some("busybox:1.36".to_string()),
                // Keeps the pod alive across multiple `exec` calls over a
                // session's lifetime; the actual work all happens via
                // exec, never via the pod's own entrypoint.
                command: Some(vec!["sleep".to_string(), "infinity".to_string()]),
                resources: Some(ResourceRequirements {
                    limits: Some(limits),
                    ..Default::default()
                }),
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_client() -> kube::Client {
        kube::Client::try_default().await.expect("KUBECONFIG must point at a reachable cluster for sandbox tests")
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

    #[tokio::test]
    async fn test_create_reaches_running_from_clean_slate() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("create");

        let sandbox = manager.create(&session_id).await.expect("create should succeed");

        let pods = pods_api(&client);
        let pod = pods.get(&sandbox.pod_name).await.expect("pod should exist");
        assert_eq!(pod.status.and_then(|s| s.phase).as_deref(), Some("Running"));

        pods.delete(&sandbox.pod_name, &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);
    }

    #[tokio::test]
    async fn test_create_reuses_existing_running_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("reuse");

        let first = manager.create(&session_id).await.expect("first create should succeed");
        let second = manager.create(&session_id).await.expect("second create should reuse, not error");

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
        let sandbox = manager.create(&session_id).await.expect("create should succeed");

        let result = sandbox.exec(&["echo", "hello"]).await.expect("exec should succeed");
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);

        let failing = sandbox.exec(&["sh", "-c", "exit 7"]).await.expect("exec should succeed even for nonzero exit");
        assert_eq!(failing.exit_code, 7);

        let pods = pods_api(&client);
        pods.delete(&sandbox.pod_name, &immediate_delete_params()).await.ok();
        std::mem::forget(sandbox);
    }

    #[tokio::test]
    async fn test_manager_delete_removes_the_pod() {
        let client = test_client().await;
        let manager = SandboxManager::new(client.clone());
        let session_id = unique_session_id("delete");
        let sandbox = manager.create(&session_id).await.expect("create should succeed");
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
        let sandbox = manager.create(&session_id).await.expect("create should succeed");
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
