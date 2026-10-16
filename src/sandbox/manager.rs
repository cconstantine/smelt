//! The process-wide `SandboxManager` and each pod's lifecycle: create,
//! start, terminate, stop, teardown, list, and volumes.

use super::*;
use k8s_openapi::api::core::v1::Event;
use k8s_openapi::jiff::Timestamp;

/// The Kubernetes client every sandbox operation uses.
pub(crate) fn kube_client() -> Result<kube::Client, SandboxError> {
    Ok(get()?.client.clone())
}

pub struct SandboxManager {
    pub(super) client: kube::Client,
    pub(super) cleanup_tx: mpsc::UnboundedSender<String>,
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
        self.create_with_docker(session_id, memory, &docker, volumes, &test_instance()).await
    }

    /// `memory` is an already-resolved value (the caller's own
    /// override, or its default) — only used on the actual-creation
    /// branch below; the reuse branch has nothing to apply them to, since
    /// resources are immutable on an already-existing pod. The same goes
    /// for `docker`, the Docker sidecar's limits and storage (SME-33).
    /// `volumes` is every currently-configured `sandbox_volumes` row —
    /// every pod gets every one of them mounted, unconditionally (see
    /// SME-17's Phase 4); an empty slice is fine for callers (mostly
    /// tests) that don't care. `instance` is the database's (SME-115):
    /// the pod is labelled with it.
    pub async fn create_with_docker(
        &self,
        session_id: &str,
        memory: &str,
        docker: &DockerSidecar,
        volumes: &[db::SandboxVolume],
        instance: &db::SmeltInstance,
    ) -> Result<Sandbox, SandboxError> {
        self.create_with_running_timeout(
            session_id,
            memory,
            docker,
            volumes,
            running_wait_timeout(),
            instance,
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
    pub(super) async fn create_with_running_timeout(
        &self,
        session_id: &str,
        memory: &str,
        docker: &DockerSidecar,
        volumes: &[db::SandboxVolume],
        running_timeout: Duration,
        instance: &db::SmeltInstance,
    ) -> Result<Sandbox, SandboxError> {
        let pods = pods_api(&self.client);
        let name = format!("sandbox-{session_id}");

        // Tracks whether *this call* created the pod, as opposed to
        // reusing one that already existed — only a pod we just created is
        // ours to delete if it never reaches `Running` below; a reused
        // pod's non-`Running` status could be a transient blip on
        // something another part of the system still depends on.
        let just_created = match pods.get_opt(&name).await? {
            // Never another database's pod (SME-115): its agent runs
            // commands for whoever connects.
            Some(pod) if ownership(&pod.metadata, &instance.id) != Ownership::Ours => {
                return Err(SandboxError::NotOurs {
                    name,
                    ownership: ownership(&pod.metadata, &instance.id),
                });
            }
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
                ensure_volume_claims(&self.client, volumes, instance).await?;
                pods.create(
                    &PostParams::default(),
                    &build_pod_spec(&name, memory, docker, volumes, &instance.id),
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
                // does) collides with it on every subsequent attempt.
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

    /// Deletes the pod now rather than through `Sandbox`'s `Drop`, which
    /// queues the same cleanup: for a pod whose start failed after it was
    /// made (its git install), and for tests that assert on the pod's
    /// absence right after deleting it, without racing the cleanup queue.
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

pub(super) async fn drain_cleanup_queue(client: kube::Client, mut rx: mpsc::UnboundedReceiver<String>) {
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

pub(super) static MANAGER: OnceLock<SandboxManager> = OnceLock::new();

/// Builds the process-wide manager from the environment's Kubernetes
/// config (`KUBECONFIG`), or returns the one already built.
pub async fn init() -> Result<&'static SandboxManager, SandboxError> {
    // `main()` already calls this before `init()` — but `init()` is also
    // called directly by `browser_tests`' in-process test harness,
    // which never runs through `main()` at all. Idempotent (`let _ = ...`
    // ignores the "already installed" error), so calling it again here is
    // safe regardless of caller. See `main()`'s own call site for the full
    // "why ring, not aws-lc-rs" explanation.
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Some(manager) = MANAGER.get() {
        return Ok(manager);
    }
    let client = kube::Client::try_default().await?;
    // Two first calls at once: the loser's manager is dropped unused.
    let _ = MANAGER.set(SandboxManager::new(client));
    get()
}

/// The process-wide manager, or `NotInitialized` before `init()`. In a
/// test, the test's own manager once it has called `use_test_manager`.
pub fn get() -> Result<&'static SandboxManager, SandboxError> {
    #[cfg(test)]
    if let Some(manager) = TEST_MANAGER.with(std::cell::Cell::get) {
        return Ok(manager);
    }
    manager_in(&MANAGER)
}

#[cfg(test)]
thread_local! {
    /// This thread's test's manager (SME-94). Per thread because each
    /// `#[sqlx::test]` runs on a thread of its own, on a current-thread
    /// runtime that runs every task it spawns there too, and a kube client
    /// works only while the runtime that built it runs: one manager shared
    /// between tests fails with `Kube(Service(Closed))` once the test that
    /// built it ends (SME-42).
    static TEST_MANAGER: std::cell::Cell<Option<&'static SandboxManager>> = const { std::cell::Cell::new(None) };
}

/// Gives every later `get()` on this thread a manager of its own on
/// `client`, built on this test's runtime, and returns it. It's leaked: its
/// cleanup task ends with the runtime, and a test makes one.
#[cfg(test)]
pub(super) fn use_test_manager(client: kube::Client) -> &'static SandboxManager {
    // On a multi-thread runtime, a task on another worker would read
    // `MANAGER` instead, and fail as `NotInitialized` (SME-94 review 1).
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::CurrentThread,
        "use_test_manager needs a current-thread runtime, as #[sqlx::test] and #[tokio::test] give"
    );
    let manager: &'static SandboxManager = Box::leak(Box::new(SandboxManager::new(client)));
    TEST_MANAGER.with(|cell| cell.set(Some(manager)));
    manager
}

/// `get` on a given cell, so a test can read one of its own.
pub(super) fn manager_in(cell: &OnceLock<SandboxManager>) -> Result<&SandboxManager, SandboxError> {
    cell.get().ok_or(SandboxError::NotInitialized)
}

mod volumes;
mod start;
mod create;
mod terminate;
mod teardown;

pub use volumes::*;
pub(in crate::sandbox) use start::*;
pub use create::*;
pub use terminate::*;
pub use teardown::*;
