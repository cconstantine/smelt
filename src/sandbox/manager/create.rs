use super::*;

// --- Pod ---

/// The decision behind `create_pod`'s one-pod-per-conversation guard,
/// pulled out as a pure function over an already-fetched live-pod list so
/// it's unit-testable without a database or cluster — see
/// SME-11's "One pod per conversation."
pub(in crate::sandbox) fn check_pod_guard(existing: &[db::SandboxPod]) -> Result<(), SandboxError> {
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
pub(in crate::sandbox) fn resolve_pod_id(existing: &[db::SandboxPod]) -> Result<i64, TerminalError> {
    existing.first().map(|p| p.id).ok_or(TerminalError::NoPod)
}

/// Decides whether a pod is confirmed dead from its already-fetched
/// status, and what reason (if any) Kubernetes gave — pulled out as a
/// pure function over an `Option<Pod>` for the same testability reason as
/// `check_pod_guard`/`resolve_pod_id`. See SME-12's "Testing": the outer
/// `Option` is "confirmed dead or not" (`None` means genuinely
/// inconclusive — `Running`/`Pending` — not "no reason"); the inner one is
/// "did Kubernetes give a specific reason for it."
pub(in crate::sandbox) fn decide_pod_death_reason(pod: Option<Pod>) -> Option<Option<String>> {
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
pub(in crate::sandbox) async fn pod_death_reason(
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
pub(in crate::sandbox) async fn conversation_pod_id(pool: &PgPool, conversation_id: i64) -> Result<i64, TerminalError> {
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
/// In a `start_or_get_pod` span (SME-137) that says whether it reused a pod.
pub async fn start_or_get_pod(pool: &PgPool, conversation_id: i64) -> Result<i64, SandboxError> {
    use tracing::field::Empty;
    let span = tracing::info_span!(
        "start_or_get_pod",
        conversation_id,
        pod_id = Empty,
        smelt.pod.reused = Empty,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    let pool = pool.clone();
    crate::telemetry::in_span(span.clone(), async move {
        let reused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = run_pod_start(conversation_id, {
            let reused = reused.clone();
            async move {
                if let Ok(pod_id) = live_pod_id(&pool, conversation_id).await {
                    reused.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Ok(pod_id);
                }
                create_pod_now(&pool, conversation_id, PodLimitOverrides::default()).await
            }
        })
        .await;
        match &result {
            Ok(pod_id) => {
                span.record("pod_id", *pod_id);
                span.record("smelt.pod.reused", reused.load(std::sync::atomic::Ordering::Relaxed));
            }
            Err(e) => crate::telemetry::mark_error(&span, &e.to_string()),
        }
        result
    })
    .await
}

/// Runs `start` in a task of its own, holding the conversation's pod-start
/// lock (SME-51 B7). Its own task: a Stop drops the turn that asked for the
/// pod, and a start cut off partway would leave a record with no pod, or a
/// pod never given its git setup. The lock makes a second start wait for
/// the first.
pub(in crate::sandbox) async fn run_pod_start<F>(conversation_id: i64, start: F) -> Result<i64, SandboxError>
where
    F: std::future::Future<Output = Result<i64, SandboxError>> + Send + 'static,
{
    let lock = pod_start_lock(conversation_id);
    // In the caller's span, so the start's spans are its children (SME-137).
    tokio::spawn(tracing::Instrument::in_current_span(async move {
        let _starting = lock.lock().await;
        start.await
    }))
    .await
    .unwrap_or_else(|e| Err(SandboxError::StartFailed(format!("starting the sandbox failed: {e}"))))
}

/// After a failed pod start: if the conversation was deleted meanwhile,
/// removes what the start may have re-created after the delete's teardown
/// ran, its claims (SME-51 code review 1). `label_id` is the conversation
/// id its pods and claims are labelled with (the same, outside tests).
pub(in crate::sandbox) async fn clean_up_after_failed_start(pool: &PgPool, client: &kube::Client, conversation_id: i64, label_id: i64) {
    if !db::conversation_exists(pool, conversation_id).await.unwrap_or(true) {
        match db::smelt_instance(pool).await {
            Ok(instance) => teardown_conversation_with(client, label_id, &[], &instance).await,
            Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't read this database's instance to clean up"),
        }
    }
}

/// `create_pod_attempt`, cleaning up after a failure for a conversation
/// deleted while it ran.
pub(in crate::sandbox) async fn create_pod_now(
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

/// One attempt at a pod, in a `create_pod_attempt` span (SME-137).
pub(in crate::sandbox) async fn create_pod_attempt(
    pool: &PgPool,
    conversation_id: i64,
    limits: PodLimitOverrides,
) -> Result<i64, SandboxError> {
    let span = tracing::info_span!(
        "create_pod_attempt",
        conversation_id,
        otel.status_code = tracing::field::Empty,
        otel.status_description = tracing::field::Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        // Boxed: the attempt's future is large (SME-115).
        let result = Box::pin(create_pod_attempt_unspanned(pool, conversation_id, limits)).await;
        if let Err(e) = &result {
            crate::telemetry::mark_error(&span, &e.to_string());
        }
        result
    })
    .await
}

pub(in crate::sandbox) async fn create_pod_attempt_unspanned(
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
                let e = start_error(pool, row.id, SandboxError::GitSetup(e)).await;
                let _ = manager.delete(sandbox).await;
                let _ = db::terminate_sandbox_pod(pool, row.id).await;
                return Err(e);
            }
            // Its record may have been closed while it started (the
            // sandbox panel's Stop): a pod left running with no live record
            // is one nothing deletes (B1, SME-122).
            if !db::sandbox_pod_is_live(pool, row.id).await.unwrap_or(true) {
                let _ = manager.delete(sandbox).await;
                return Err(stopped_while_starting());
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
            let e = start_error(pool, row.id, e).await;
            let _ = db::terminate_sandbox_pod(pool, row.id).await;
            Err(e)
        }
    }
}

/// What a start says when its pod's record was closed while it ran (B1,
/// SME-122).
fn stopped_while_starting() -> SandboxError {
    SandboxError::StartFailed("the sandbox was stopped while it was starting".to_string())
}

/// The error a failed start reports: `stopped_while_starting` when its
/// record was closed meanwhile (the panel's Stop deletes the pod the start
/// waits for, which then fails with Kubernetes' 404), else `e` (B1,
/// SME-122). Read before the start closes the record itself.
async fn start_error(pool: &PgPool, pod_id: i64, e: SandboxError) -> SandboxError {
    if db::sandbox_pod_is_live(pool, pod_id).await.unwrap_or(true) {
        e
    } else {
        stopped_while_starting()
    }
}

/// Whether a start is under way for `pod_id`'s conversation: its start
/// lock is held from before the pod's record is made until the start
/// returns (B1, SME-122). A start that only finds the live pod holds it
/// briefly too, which just puts off a reconnect's crash check.
pub(in crate::sandbox) async fn start_under_way_for_pod(pool: &PgPool, pod_id: i64) -> bool {
    match db::sandbox_pod_conversation_id(pool, pod_id).await {
        Ok(Some(conversation_id)) => pod_start_lock(conversation_id).try_lock().is_err(),
        _ => false,
    }
}

/// Each conversation's "a pod is being started" lock (SME-51 B7).
pub(in crate::sandbox) fn pod_start_lock(conversation_id: i64) -> Arc<tokio::sync::Mutex<()>> {
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
