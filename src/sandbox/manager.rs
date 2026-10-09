//! The process-wide `SandboxManager` and each pod's lifecycle: create,
//! start, terminate, stop, teardown, list, and volumes.

use super::*;
use k8s_openapi::api::core::v1::Event;
use k8s_openapi::jiff::Timestamp;

/// Creates any volume claim that's missing from this namespace. Every pod
/// mounts every configured volume, and a pod whose claim doesn't exist
/// can never be scheduled — it just waits until the start-up timeout. A
/// claim goes missing when the cluster is rebuilt or the claim deleted,
/// or when the volume was created in another namespace (the browser tier
/// shares the dev database but runs in the test namespace). Whatever the
/// old claim held is gone either way; recreating it keeps sandboxes working.
pub(super) async fn ensure_volume_claims(
    client: &kube::Client,
    volumes: &[db::SandboxVolume],
    instance: &db::SmeltInstance,
) -> Result<(), SandboxError> {
    let pvcs = pvc_api(client);
    for volume in volumes {
        // Boxed, like `ensure_conversation_pvcs`' claims (SME-115 review 1).
        Box::pin(ensure_volume_claim(&pvcs, volume.id, instance)).await?;
    }
    Ok(())
}

/// One of `ensure_volume_claims`' claims: made if missing, and refused
/// unless it's ours.
async fn ensure_volume_claim(
    pvcs: &Api<PersistentVolumeClaim>,
    volume_id: i64,
    instance: &db::SmeltInstance,
) -> Result<(), SandboxError> {
    let claim = sandbox_volume_pvc_name(volume_id);
    match pvcs.get_opt(&claim).await? {
        None => {
            tracing::warn!(claim = %claim, "volume claim missing; recreating it");
            pvcs.create(&PostParams::default(), &build_volume_pvc_spec(volume_id, &instance.id))
                .await?;
            Ok(())
        }
        // Never another database's volume (SME-115).
        Some(existing) => match ownership(&existing.metadata, &instance.id) {
            Ownership::Ours => Ok(()),
            // One from before the fix that startup missed: its owner
            // adopts it now.
            Ownership::Unlabelled if instance.owns_unlabelled => {
                let none = std::collections::HashSet::new();
                let this_volume = std::collections::HashSet::from([volume_id]);
                let decide = |meta: &ObjectMeta| should_adopt(meta, instance, &none, &this_volume);
                if adopt_one(pvcs, existing, &instance.id, decide).await {
                    Ok(())
                } else {
                    Err(SandboxError::NotOurs { name: claim, ownership: Ownership::Unlabelled })
                }
            }
            other => Err(SandboxError::NotOurs { name: claim, ownership: other }),
        },
    }
}

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

// Only called by the "no-agent pod" real-cluster test in `tests.rs` — `create`
// itself now goes straight to `create_with_running_timeout`.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn wait_for_running(pods: &Api<Pod>, name: &str) -> Result<(), SandboxError> {
    wait_for_running_with_timeout(pods, name, running_wait_timeout()).await
}

pub(super) async fn wait_for_running_with_timeout(
    pods: &Api<Pod>,
    name: &str,
    timeout: Duration,
) -> Result<(), SandboxError> {
    let mut last_detail = None;
    let started = std::time::Instant::now();
    let waited = tokio::time::timeout(timeout, async {
        loop {
            let pod = pods.get(name).await?;
            if let Some(reason) = pod_startup_failure(&pod) {
                return Err(SandboxError::StartFailed(reason));
            }
            last_detail = pod_pending_detail(&pod);
            if pod.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running") {
                let took = started.elapsed();
                if is_slow_start(took, timeout) {
                    tracing::warn!(
                        pod = %name,
                        took_secs = took.as_secs(),
                        timeout_secs = timeout.as_secs(),
                        timeline = %pod_start_timeline(&pod),
                        "a sandbox pod took more than half its start timeout to reach Running"
                    );
                }
                return Ok(());
            }
            // Not held across the sleep (see `test_sandbox_futures_stay_small`).
            drop(pod);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    match waited {
        Ok(result) => result,
        // Boxed: a `Pod` and its events are large, and this path is rare
        // (see `test_sandbox_futures_stay_small`).
        Err(_) => Err(SandboxError::Timeout(Box::pin(timed_out_detail(pods, name, last_detail)).await)),
    }
}

/// Whether a start that took `took` came close enough to `timeout` to log.
/// A start past half its timeout is logged (SME-132), so a dev server's
/// log shows how close real starts come to the limit.
pub(super) fn is_slow_start(took: Duration, timeout: Duration) -> bool {
    took > timeout / 2
}

/// How long `timed_out_detail` waits for each of its two reads.
const REPORT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A timed-out start's detail: `pod_start_report` on a fresh read of the
/// pod and its events (SME-132). It runs before `create_with_running_timeout`
/// deletes the pod. When the pod can't be read, the last poll's
/// `pod_pending_detail` stands.
async fn timed_out_detail(pods: &Api<Pod>, name: &str, last_detail: Option<String>) -> Option<String> {
    let Ok(Ok(pod)) = tokio::time::timeout(REPORT_READ_TIMEOUT, pods.get(name)).await else {
        return last_detail;
    };
    let events = pod_events(pods, &pod).await;
    Some(pod_start_report(&pod, events.as_deref().map_err(String::as_str), Timestamp::now()))
}

/// `pod`'s events, or why they couldn't be listed. The `park` Role may
/// list events in both namespaces (`k8s/smelt-park-rbac.yaml`).
async fn pod_events(pods: &Api<Pod>, pod: &Pod) -> Result<Vec<Event>, String> {
    let client = pods.clone().into_client();
    let events: Api<Event> = match pods.namespace() {
        Some(namespace) => Api::namespaced(client, namespace),
        None => Api::all(client),
    };
    let mut selector = format!("involvedObject.name={}", pod.metadata.name.as_deref().unwrap_or_default());
    if let Some(uid) = &pod.metadata.uid {
        selector.push_str(&format!(",involvedObject.uid={uid}"));
    }
    match tokio::time::timeout(REPORT_READ_TIMEOUT, events.list(&ListParams::default().fields(&selector))).await {
        Ok(Ok(list)) => Ok(list.items),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no answer in {} s", REPORT_READ_TIMEOUT.as_secs())),
    }
}

/// The most events `pod_start_report` lists.
const REPORT_MAX_EVENTS: usize = 5;

/// The most characters of an event's message `pod_start_report` keeps.
const REPORT_MAX_MESSAGE_CHARS: usize = 200;

/// What a pod that hasn't started is stuck on, for a timed-out wait's
/// error (SME-132): the stage (its first `False` condition, as
/// `pod_pending_detail` gives it) and for how long, the pod's age, the
/// stages it passed, each container's state, and up to 5 of its events,
/// `Warning`s first, then the newest. `events` is `Err` with why when
/// they couldn't be listed; the rest is reported anyway.
pub(super) fn pod_start_report(pod: &Pod, events: Result<&[Event], &str>, now: Timestamp) -> String {
    let created = pod.metadata.creation_timestamp.as_ref().map(|t| t.0);
    let conditions = pod.status.as_ref().and_then(|s| s.conditions.as_deref()).unwrap_or_default();
    let stage = match conditions.iter().find(|c| c.status == "False") {
        Some(stuck) => {
            let detail = pod_pending_detail(pod).unwrap_or_else(|| stuck.type_.clone());
            let gloss = stage_gloss(&stuck.type_).map(|g| format!(" ({g})")).unwrap_or_default();
            let since = stuck.last_transition_time.as_ref().map(|t| format!(" for {}", whole_seconds(t.0, now)));
            format!("{detail}{gloss}{}", since.unwrap_or_default())
        }
        None if conditions.is_empty() => "not scheduled yet".to_string(),
        None => "no stage false".to_string(),
    };
    let mut parts = vec![stage];
    if let Some(created) = created {
        parts.push(format!("pod {} old", whole_seconds(created, now)));
    }
    let timeline = pod_start_timeline(pod);
    if !timeline.is_empty() {
        parts.push(format!("timeline: {timeline}"));
    }
    parts.push(format!("containers: {}", container_states(pod)));
    parts.push(match events {
        Ok([]) => "events: none".to_string(),
        Ok(events) => format!("events: {}", describe_events(events)),
        Err(e) => format!("events unavailable: {e}"),
    });
    parts.join("; ")
}

/// What the kubelet does while one of its pod conditions is `False`.
fn stage_gloss(condition: &str) -> Option<&'static str> {
    match condition {
        "PodReadyToStartContainers" => Some("volumes and sandbox"),
        "Initialized" => Some("Docker sidecar starting"),
        "ContainersReady" | "Ready" => Some("sandbox container starting"),
        _ => None,
    }
}

/// `from` to `to` in whole seconds, as "88 s". The API's timestamps are
/// to the second. Never negative: `to` may be smelt's clock and `from` the
/// node's, and a skew between them would otherwise show as "-3 s"
/// (SME-132 review 1).
fn whole_seconds(from: Timestamp, to: Timestamp) -> String {
    format!("{} s", to.duration_since(from).as_secs().max(0))
}

/// Each condition that went `True`, as an offset from the pod's creation,
/// in the order they happened: "scheduled +0 s, sandbox ready +1 s".
fn pod_start_timeline(pod: &Pod) -> String {
    let Some(created) = pod.metadata.creation_timestamp.as_ref().map(|t| t.0) else {
        return String::new();
    };
    let conditions = pod.status.as_ref().and_then(|s| s.conditions.as_deref()).unwrap_or_default();
    let mut passed: Vec<_> = conditions
        .iter()
        .filter(|c| c.status == "True")
        .filter_map(|c| Some((c.last_transition_time.as_ref()?.0, c.type_.as_str())))
        .collect();
    // Times are whole seconds, so stages tie often; a tie goes in the
    // order the kubelet passes them, not the API's list order (SME-132
    // review 1). Conditions it doesn't know go after, by name.
    passed.sort_by_key(|(at, condition)| (*at, stage_rank(condition), *condition));
    passed
        .iter()
        .map(|(at, condition)| {
            let name = match *condition {
                "PodScheduled" => "scheduled",
                "PodReadyToStartContainers" => "sandbox ready",
                "Initialized" => "sidecar started",
                "ContainersReady" => "containers ready",
                "Ready" => "ready",
                other => other,
            };
            format!("{name} +{}", whole_seconds(created, *at))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where a pod condition comes in a start, for ordering stages that
/// happened in the same second.
fn stage_rank(condition: &str) -> u8 {
    match condition {
        "PodScheduled" => 0,
        "PodReadyToStartContainers" => 1,
        "Initialized" => 2,
        "ContainersReady" => 3,
        "Ready" => 4,
        _ => 5,
    }
}

/// Each of the pod spec's containers, init containers first, with its
/// state: "docker running, not started, sandbox not created".
fn container_states(pod: &Pod) -> String {
    let spec = pod.spec.as_ref();
    let status = pod.status.as_ref();
    let init = spec.and_then(|s| s.init_containers.as_deref()).unwrap_or_default();
    let main = spec.map(|s| s.containers.as_slice()).unwrap_or_default();
    let statuses = [
        status.and_then(|s| s.init_container_statuses.as_deref()).unwrap_or_default(),
        status.and_then(|s| s.container_statuses.as_deref()).unwrap_or_default(),
    ];
    init.iter()
        .chain(main)
        .map(|container| {
            let found = statuses.iter().flat_map(|s| s.iter()).find(|s| s.name == container.name);
            format!("{} {}", container.name, found.map_or_else(|| "not created".to_string(), container_state))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One container's state, as `container_states` writes it.
fn container_state(status: &k8s_openapi::api::core::v1::ContainerStatus) -> String {
    let state = status.state.as_ref();
    if let Some(waiting) = state.and_then(|s| s.waiting.as_ref()) {
        return format!("waiting ({})", waiting.reason.as_deref().unwrap_or("no reason"));
    }
    if state.and_then(|s| s.running.as_ref()).is_some() {
        return match status.started {
            Some(false) => "running, not started".to_string(),
            _ => "running".to_string(),
        };
    }
    if let Some(done) = state.and_then(|s| s.terminated.as_ref()) {
        return format!("terminated ({}, exit {})", done.reason.as_deref().unwrap_or("no reason"), done.exit_code);
    }
    "not created".to_string()
}

/// Up to `REPORT_MAX_EVENTS` events, `Warning`s first and then the newest,
/// as "FailedMount (×4): …", each message cut to
/// `REPORT_MAX_MESSAGE_CHARS`.
fn describe_events(events: &[Event]) -> String {
    let last_seen = |e: &Event| {
        e.last_timestamp
            .as_ref()
            .map(|t| t.0)
            .or_else(|| e.event_time.as_ref().map(|t| t.0))
            .or_else(|| e.first_timestamp.as_ref().map(|t| t.0))
    };
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|e| (e.type_.as_deref() != Some("Warning"), std::cmp::Reverse(last_seen(e))));
    sorted
        .iter()
        .take(REPORT_MAX_EVENTS)
        .map(|e| {
            let message = e.message.as_deref().unwrap_or("");
            let mut cut: String = message.chars().take(REPORT_MAX_MESSAGE_CHARS).collect();
            if cut.len() < message.len() {
                cut.push('…');
            }
            format!("{} (×{}): {cut}", e.reason.as_deref().unwrap_or("no reason"), e.count.unwrap_or(1))
        })
        .collect::<Vec<_>>()
        .join(" | ")
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
    // called directly by `browser_tests.rs`'s in-process test harness,
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

// --- Pod ---

/// The decision behind `create_pod`'s one-pod-per-conversation guard,
/// pulled out as a pure function over an already-fetched live-pod list so
/// it's unit-testable without a database or cluster — see
/// SME-11's "One pod per conversation."
pub(super) fn check_pod_guard(existing: &[db::SandboxPod]) -> Result<(), SandboxError> {
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
pub(super) fn resolve_pod_id(existing: &[db::SandboxPod]) -> Result<i64, TerminalError> {
    existing.first().map(|p| p.id).ok_or(TerminalError::NoPod)
}

/// Decides whether a pod is confirmed dead from its already-fetched
/// status, and what reason (if any) Kubernetes gave — pulled out as a
/// pure function over an `Option<Pod>` for the same testability reason as
/// `check_pod_guard`/`resolve_pod_id`. See SME-12's "Testing": the outer
/// `Option` is "confirmed dead or not" (`None` means genuinely
/// inconclusive — `Running`/`Pending` — not "no reason"); the inner one is
/// "did Kubernetes give a specific reason for it."
pub(super) fn decide_pod_death_reason(pod: Option<Pod>) -> Option<Option<String>> {
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
/// Another database's pod of that name reads as gone (SME-115).
pub(super) async fn pod_death_reason(
    pods: &Api<Pod>,
    name: &str,
    instance: &db::SmeltInstance,
) -> Option<Option<String>> {
    match read_our_pod(pods, name, instance).await {
        Ok(pod) => decide_pod_death_reason(pod),
        Err(_) => None,
    }
}

/// Resolves "the conversation's pod" — with `create_pod`'s guard in place,
/// a conversation has at most one live pod, so every pod-scoped call
/// (`terminate_pod`, `create_terminal`, the file tools) can go straight
/// from a `conversation_id` to a `pod_id` without the model ever naming one
/// itself. See SME-11's "One pod per conversation."
pub(super) async fn conversation_pod_id(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
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
pub(super) async fn run_pod_start<F>(conversation_id: i64, start: F) -> Result<i64, SandboxError>
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
pub(super) async fn clean_up_after_failed_start(pool: &PgPool, client: &kube::Client, conversation_id: i64, label_id: i64) {
    if !db::conversation_exists(pool, conversation_id).await.unwrap_or(true) {
        match db::smelt_instance(pool).await {
            Ok(instance) => teardown_conversation_with(client, label_id, &[], &instance).await,
            Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't read this database's instance to clean up"),
        }
    }
}

/// `create_pod_attempt`, cleaning up after a failure for a conversation
/// deleted while it ran.
pub(super) async fn create_pod_now(
    pool: &PgPool,
    conversation_id: i64,
    limits: PodLimitOverrides,
) -> Result<i64, SandboxError> {
    // Boxed: the attempt's future is large, and a caller awaiting it in
    // a debug build holds it on its own stack frame (SME-115).
    let result = Box::pin(create_pod_attempt(pool, conversation_id, limits)).await;
    // Only a deleted conversation needs the cluster touched; a refused
    // start for a live one (a pod already exists, say) doesn't.
    if result.is_err() && !db::conversation_exists(pool, conversation_id).await.unwrap_or(true) {
        if let Ok(manager) = get() {
            clean_up_after_failed_start(pool, &manager.client, conversation_id, conversation_id).await;
        }
    }
    result
}

pub(super) async fn create_pod_attempt(
    pool: &PgPool,
    conversation_id: i64,
    limits: PodLimitOverrides,
) -> Result<i64, SandboxError> {
    let existing = db::list_sandbox_pods(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    check_pod_guard(&existing)?;

    let (memory, docker) = limits.resolve(conversation_id);

    let manager = get()?;
    let instance = db::smelt_instance(pool).await.map_err(SandboxError::Db)?;
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
        wait_for_conversation_pods_gone(&manager.client, conversation_id, running_wait_timeout(), &instance.id)
            .await
    {
        let _ = db::terminate_sandbox_pod(pool, row.id).await;
        return Err(e);
    }
    if let Err(e) = ensure_conversation_pvcs(&manager.client, conversation_id, &instance).await {
        let _ = db::terminate_sandbox_pod(pool, row.id).await;
        return Err(e);
    }
    match manager
        .create_with_docker(&row.id.to_string(), &memory, &docker, &volumes, &instance)
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
                teardown_conversation_with(&manager.client, conversation_id, &[], &instance).await;
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
pub(super) fn pod_start_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<StdMutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(conversation_id)
        .or_default()
        .clone()
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
pub(super) async fn force_terminate_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<db::SandboxPod>, SandboxError> {
    terminate_pod_with(pool, pod_id, async { Box::pin(delete_terminated_pod(pool, &get()?.client, pod_id)).await }).await
}

/// `force_terminate_pod` on `client`.
pub(super) async fn force_terminate_pod_with(
    pool: &PgPool,
    client: &kube::Client,
    pod_id: i64,
) -> Result<Option<db::SandboxPod>, SandboxError> {
    terminate_pod_with(pool, pod_id, Box::pin(delete_terminated_pod(pool, client, pod_id))).await
}

/// The cluster's part of terminating `pod_id`: deletes its pod if it's
/// still there and ours (SME-115). Another database's pod of that name,
/// or one from before the fix, is left; its record is closed all the same.
async fn delete_terminated_pod(pool: &PgPool, client: &kube::Client, pod_id: i64) -> Result<(), SandboxError> {
    let instance = db::smelt_instance(pool).await.map_err(SandboxError::Db)?;
    let pods = pods_api(client);
    let name = pod_name(pod_id);
    // Only its ownership and uid are kept: a whole `Pod` held across the
    // delete below made every caller's future twice the size, which
    // overflowed a debug build's stack (SME-115).
    let (mut owner, mut uid) = match pods.get_opt(&name).await? {
        Some(pod) => (ownership(&pod.metadata, &instance.id), pod.metadata.uid),
        None => return Ok(()),
    };
    // One from before SME-115 that startup's adoption missed: adopted now
    // if it's this record's conversation's, so it's stopped rather than
    // left running with its record closed (SME-115 review 1).
    if owner == Ownership::Unlabelled && instance.owns_unlabelled {
        match Box::pin(adopt_record_pod(pool, &pods, pod_id, &instance)).await {
            RecordPodAdoption::Adopted => {
                (owner, uid) = match pods.get_opt(&name).await? {
                    Some(pod) => (ownership(&pod.metadata, &instance.id), pod.metadata.uid),
                    None => return Ok(()),
                };
            }
            RecordPodAdoption::NotOurs => {}
            // Ours but not labelled: an error keeps the record open, so
            // the pod isn't left running with no record (SME-115 review 2).
            RecordPodAdoption::Failed => {
                return Err(SandboxError::NotOurs { name, ownership: Ownership::Unlabelled });
            }
        }
    }
    match owner {
        Ownership::Ours => {
            let params = DeleteParams {
                preconditions: Some(kube::api::Preconditions { uid, resource_version: None }),
                ..pod_delete_params()
            };
            match pods.delete(&name, &params).await {
                Ok(_) => {
                    // The record of what this server deleted (SME-117).
                    let uid = params.preconditions.as_ref().and_then(|p| p.uid.as_deref()).unwrap_or_default();
                    tracing::info!(pod = %name, %uid, "deleted pod");
                    Ok(())
                }
                // Gone, or replaced since it was read: not ours to delete.
                Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
        other => {
            tracing::warn!(pod = %name, ownership = ?other, "closed the record of a pod that isn't this database's; the pod is left");
            Ok(())
        }
    }
}

/// `name`'s pod if it counts as this database's (`counts_as_ours`);
/// `None` when there's none, or it's another database's.
pub(super) async fn read_our_pod(
    pods: &Api<Pod>,
    name: &str,
    instance: &db::SmeltInstance,
) -> Result<Option<Pod>, kube::Error> {
    Ok(pods.get_opt(name).await?.filter(|pod| counts_as_ours(&pod.metadata, instance)))
}

/// `force_terminate_pod` with the cluster's delete passed in, so a test can
/// make it fail.
pub(super) async fn terminate_pod_with(
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

/// `pod_id`'s phase and limits, or `None` if Kubernetes has no such pod
/// of this database's (SME-115).
pub async fn pod_details(pool: &PgPool, pod_id: i64) -> Result<Option<PodDetails>, SandboxError> {
    let pods = pods_api(&get()?.client);
    let instance = db::smelt_instance(pool).await.map_err(SandboxError::Db)?;
    let Some(pod) = read_our_pod(&pods, &pod_name(pod_id), &instance).await? else {
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
    let request = pod_metrics_request(NAMESPACE)?;
    Ok(get()?.client.request::<serde_json::Value>(request).await?)
}

/// The metrics API request for `namespace`'s pods. `NAMESPACE` is always a
/// valid path segment, but the builder's error is returned rather than
/// unwrapped (SME-95).
pub(super) fn pod_metrics_request(namespace: &str) -> Result<http::Request<Vec<u8>>, SandboxError> {
    http::Request::get(format!("/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods"))
        .body(Vec::new())
        .map_err(|e| SandboxError::Kube(kube::Error::HttpError(e)))
}

pub async fn list_pods(pool: &PgPool, conversation_id: i64) -> Result<Vec<PodInfo>, SandboxError> {
    let manager = get()?;
    let pods = pods_api(&manager.client);
    let rows = db::list_sandbox_pods(pool, conversation_id)
        .await
        .map_err(SandboxError::Db)?;
    let instance = db::smelt_instance(pool).await.map_err(SandboxError::Db)?;
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let name = pod_name(row.id);
        let status = read_our_pod(&pods, &name, &instance)
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
    // Before the row, so a missing manager leaves nothing behind.
    let manager = get()?;
    let instance = db::smelt_instance(pool).await.map_err(SandboxError::Db)?;
    let row = db::create_sandbox_volume(pool, name, &resolved_path)
        .await
        .map_err(SandboxError::Db)?;

    let pvcs = pvc_api(&manager.client);
    if let Err(e) = pvcs
        .create(&PostParams::default(), &build_volume_pvc_spec(row.id, &instance.id))
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
    let manager = get()?;
    let instance = db::smelt_instance(pool).await.map_err(SandboxError::Db)?;
    delete_volume_claim(&manager.client, id, &instance).await?;
    db::delete_sandbox_volume(pool, id)
        .await
        .map_err(SandboxError::Db)?;
    Ok(())
}

/// Deletes volume `id`'s claim if it's ours (SME-115), only while it's
/// still the object read. Another database's claim of that name, or one
/// from before the fix, is left and logged.
pub(super) async fn delete_volume_claim(
    client: &kube::Client,
    id: i64,
    instance: &db::SmeltInstance,
) -> Result<(), SandboxError> {
    let pvcs = pvc_api(client);
    let name = sandbox_volume_pvc_name(id);
    let Some(mut claim) = pvcs.get_opt(&name).await? else {
        return Ok(());
    };
    // One from before SME-115 that startup missed: its owner adopts it
    // first, or the volume's data would stay in the cluster for good once
    // its row is gone (SME-115 review 2). Failing that, the row stays.
    if instance.owns_unlabelled && ownership(&claim.metadata, &instance.id) == Ownership::Unlabelled {
        let none = std::collections::HashSet::new();
        let this_volume = std::collections::HashSet::from([id]);
        let decide = |meta: &ObjectMeta| should_adopt(meta, instance, &none, &this_volume);
        if !Box::pin(adopt_one(&pvcs, claim, &instance.id, decide)).await {
            return Err(SandboxError::NotOurs { name, ownership: Ownership::Unlabelled });
        }
        claim = match pvcs.get_opt(&name).await? {
            Some(claim) => claim,
            None => return Ok(()),
        };
    }
    let instance = instance.id.as_str();
    match (ownership(&claim.metadata, instance), claim.metadata.uid) {
        (Ownership::Ours, Some(uid)) => {
            let params = DeleteParams {
                preconditions: Some(kube::api::Preconditions { uid: Some(uid), resource_version: None }),
                ..Default::default()
            };
            match pvcs.delete(&name, &params).await {
                Ok(_) => {
                    // The record of what this server deleted (SME-117).
                    let uid = params.preconditions.as_ref().and_then(|p| p.uid.as_deref()).unwrap_or_default();
                    tracing::info!(claim = %name, %uid, "deleted volume claim");
                    Ok(())
                }
                // Gone, or replaced since it was read: not ours to delete.
                Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
        (other, _) => {
            tracing::warn!(claim = %name, ownership = ?other, "left a volume claim that isn't this database's");
            Ok(())
        }
    }
}

/// Whether sandbox pod `pod_id` still exists in the cluster (terminating
/// counts as existing).
#[cfg(all(test, feature = "browser-test"))]
pub(crate) async fn pod_exists(pod_id: i64) -> bool {
    let Ok(manager) = get() else {
        return false;
    };
    matches!(
        pods_api(&manager.client).get_opt(&pod_name(pod_id)).await,
        Ok(Some(_))
    )
}

/// Deletes every pod that exists for this conversation, unconditionally
/// (unlike `terminate_pod`, this is a hard teardown on conversation
/// deletion, not a guarded API the model calls) — see SME-9's
/// `chat.rs`/`main.rs` bullet. The DB rows themselves don't need clearing
/// here: `db::delete_conversation`'s `ON DELETE CASCADE` chain removes
/// `sandbox_pods`/`sandbox_terminals`/`terminal_commands` for real right
/// after this runs.
pub async fn teardown_conversation(pool: &PgPool, conversation_id: i64, pod_ids: &[i64]) {
    let manager = match get() {
        Ok(manager) => manager,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't tear down the conversation's sandbox");
            return;
        }
    };
    // Without it nothing can be told ours: leave it all (SME-115). The
    // startup sweep gets the claims; the pods are logged.
    match db::smelt_instance(pool).await {
        Ok(instance) => teardown_conversation_with(&manager.client, conversation_id, pod_ids, &instance).await,
        Err(e) => tracing::warn!(
            conversation_id,
            ?pod_ids,
            error = %e,
            "couldn't read this database's instance; the conversation's sandbox is left running"
        ),
    }
}

/// `teardown_conversation` on `client`. Pods are found by their
/// conversation label: a `create_pod` racing the conversation's deletion
/// makes a pod whose record the delete then cascades away (SME-51 B5).
/// `pod_ids`, the conversation's live pod records read before the delete,
/// name the rest (SME-88). Only objects labelled with `instance` are
/// deleted: another database's conversation of the same id, or one from
/// before SME-115, is left alone.
pub(super) async fn teardown_conversation_with(
    client: &kube::Client,
    conversation_id: i64,
    pod_ids: &[i64],
    instance: &db::SmeltInstance,
) {
    // Before anything is deleted: the lines below are matched to it
    // (SME-117).
    tracing::info!(conversation_id, instance = %instance.id, ?pod_ids, "tearing down a conversation's sandbox");
    let pods = pods_api(client);
    // What startup's adoption missed is this conversation's too: adopted
    // now, by the database that owns such objects, so it goes below
    // instead of being left for good (SME-115 review 1).
    if instance.owns_unlabelled {
        Box::pin(adopt_conversation_objects(client, conversation_id, pod_ids, instance)).await;
    }
    let instance = instance.id.as_str();
    // Its sandbox pods, and its language server pods (SME-35).
    for label in [CONVERSATION_LABEL, crate::lsp::pods::LSP_OF_LABEL] {
        let selector =
            ListParams::default().labels(&format!("{label}={conversation_id},{INSTANCE_LABEL}={instance}"));
        delete_listed(&pods, &selector, conversation_id, instance).await;
    }
    // And the pods its records name, without a conversation label (one
    // from before SME-33; SME-88). A pod deleted above is already gone.
    for &pod_id in pod_ids {
        deregister(pod_id);
        delete_pod_if_ours(&pods, &pod_name(pod_id), instance).await;
    }
    // After the pods: Kubernetes holds a claim until no pod mounts it.
    delete_conversation_pvcs(client, conversation_id, instance).await;
}

pub(super) async fn delete_listed(pods: &Api<Pod>, selector: &ListParams, conversation_id: i64, instance: &str) {
    match pods.list(selector).await {
        Ok(list) => {
            for pod in list {
                // Whatever the selector: only ours (SME-115).
                if ownership(&pod.metadata, instance) != Ownership::Ours {
                    continue;
                }
                if let Some(pod_id) = watched_pod_id(&pod) {
                    deregister(pod_id);
                }
                // Already being deleted: nothing to do, or to log again
                // (SME-117 review 1).
                if pod.metadata.deletion_timestamp.is_some() {
                    continue;
                }
                let (Some(name), Some(uid)) = (pod.metadata.name, pod.metadata.uid) else { continue };
                delete_pod_if_unchanged(pods, &name, &uid).await;
            }
        }
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't list a deleted conversation's pods"),
    }
}

/// Deletes pod `name` if it's ours (SME-115), only while it's still the
/// object that was read. Another database's pod of the same name, or one
/// from before the fix, is left and logged. Best-effort.
pub(super) async fn delete_pod_if_ours(pods: &Api<Pod>, name: &str, instance: &str) {
    // Only its ownership and uid are kept across the delete (see
    // `delete_terminated_pod`).
    let (owner, uid) = match pods.get_opt(name).await {
        // Already being deleted (by the label pass of the same teardown,
        // say): nothing to do, or to log again (SME-117 review 1).
        Ok(Some(pod)) if pod.metadata.deletion_timestamp.is_some() => return,
        Ok(Some(pod)) => (ownership(&pod.metadata, instance), pod.metadata.uid),
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(pod = %name, error = %e, "couldn't read a pod to delete");
            return;
        }
    };
    match (owner, uid) {
        (Ownership::Ours, Some(uid)) => delete_pod_if_unchanged(pods, name, &uid).await,
        (Ownership::Ours, None) => tracing::warn!(pod = %name, "a pod with no uid; left alone"),
        (Ownership::Unlabelled, _) => {
            tracing::warn!(pod = %name, "left a pod with no smelt/instance label (made before SME-115)")
        }
        (Ownership::Foreign, _) => tracing::info!(pod = %name, "left another smelt database's pod of the same name"),
    }
}

/// Deletes pod `name` with smelt's grace period, only while it's still
/// the object with `uid`. Best-effort: logged, never returned.
pub(super) async fn delete_pod_if_unchanged(pods: &Api<Pod>, name: &str, uid: &str) {
    let params = DeleteParams {
        preconditions: Some(kube::api::Preconditions {
            uid: Some(uid.to_string()),
            resource_version: None,
        }),
        ..pod_delete_params()
    };
    match pods.delete(name, &params).await {
        // The record of what this server deleted (SME-117).
        Ok(_) => tracing::info!(pod = %name, %uid, "deleted pod"),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {
            tracing::info!(pod = %name, "a pod was replaced since it was read; left alone")
        }
        Err(e) => tracing::warn!(pod = %name, error = %e, "failed to delete a pod"),
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
        crate::turn::notify(pool, conversation_id, vec![crate::turn::Notice::Save(notice)]);
    }
    Ok(())
}
