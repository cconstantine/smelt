use super::*;

/// Takes `conversation_id`, resolved to "the conversation's pod" via
/// `conversation_pod_id` — no longer idempotent on repeat the way a
/// `pod_id`-addressed version was: once the one live pod is terminated,
/// there's no longer a live pod for this conversation to resolve, so a
/// second call fails clearly with `NoPod` ("call create_pod first") rather
/// than silently succeeding again. See SME-11's "How." Refuses if the
/// pod still has a live terminal.
/// In a `terminate_pod` span (SME-137).
pub async fn terminate_pod(pool: &PgPool, conversation_id: i64) -> Result<(), TerminalError> {
    let span = tracing::info_span!(
        "terminate_pod",
        conversation_id,
        otel.status_code = tracing::field::Empty,
        otel.status_description = tracing::field::Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        let result = terminate_pod_unspanned(pool, conversation_id).await;
        if let Err(e) = &result {
            crate::telemetry::mark_error(&span, &e.to_string());
        }
        result
    })
    .await
}

async fn terminate_pod_unspanned(pool: &PgPool, conversation_id: i64) -> Result<(), TerminalError> {
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
pub(in crate::sandbox) async fn force_terminate_pod(
    pool: &PgPool,
    pod_id: i64,
) -> Result<Option<db::SandboxPod>, SandboxError> {
    terminate_pod_with(pool, pod_id, async { Box::pin(delete_terminated_pod(pool, &get()?.client, pod_id)).await }).await
}

/// `force_terminate_pod` on `client`.
pub(in crate::sandbox) async fn force_terminate_pod_with(
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
pub(in crate::sandbox) async fn read_our_pod(
    pods: &Api<Pod>,
    name: &str,
    instance: &db::SmeltInstance,
) -> Result<Option<Pod>, kube::Error> {
    Ok(pods.get_opt(name).await?.filter(|pod| counts_as_ours(&pod.metadata, instance)))
}

/// `force_terminate_pod` with the cluster's delete passed in, so a test can
/// make it fail.
pub(in crate::sandbox) async fn terminate_pod_with(
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
pub(in crate::sandbox) fn pod_metrics_request(namespace: &str) -> Result<http::Request<Vec<u8>>, SandboxError> {
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
