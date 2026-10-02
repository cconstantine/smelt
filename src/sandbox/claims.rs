//! A conversation's own claims (`/workspace`, Docker's data), their orphan
//! sweep, and Docker sidecar restarts.

use super::*;

/// The conversations whose Docker data claims outlived them: claims
/// labelled with a conversation that isn't in `live`.
pub(super) fn orphaned_docker_claims(
    claims: &[PersistentVolumeClaim],
    live: &std::collections::HashSet<i64>,
) -> Vec<i64> {
    claims
        .iter()
        .filter_map(|c| c.metadata.labels.as_ref()?.get(CONVERSATION_LABEL)?.parse().ok())
        .filter(|id| !live.contains(id))
        .collect::<std::collections::BTreeSet<i64>>()
        .into_iter()
        .collect()
}

/// Deletes Docker data claims whose conversation is gone. Conversation
/// teardown deletes its claim best-effort, so this catches what that
/// missed. Run once at startup by `main`, never from tests: tests share
/// the namespace but each has its own database, so from a test every
/// other test's claim would look orphaned.
pub async fn sweep_orphaned_conversation_claims(pool: &PgPool) {
    let client = &get().client;
    let selector = ListParams::default().labels(CONVERSATION_LABEL);
    // Claims first, then conversations: a claim only exists for a
    // conversation that already did, so none can be missed in between.
    let claims = match pvc_api(client).list(&selector).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list docker data claims to sweep");
            return;
        }
    };
    let live = match db::list_conversations(pool).await {
        Ok(conversations) => conversations.into_iter().map(|c| c.id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list conversations to sweep docker data claims");
            return;
        }
    };
    for conversation_id in orphaned_docker_claims(&claims, &live) {
        tracing::info!(conversation_id, "deleting a deleted conversation's docker data claim");
        delete_conversation_pvcs(client, conversation_id).await;
    }
}

/// The pod's Docker sidecar's status, if it has one yet.
pub(super) fn docker_status(pod: &Pod) -> Option<&k8s_openapi::api::core::v1::ContainerStatus> {
    pod.status
        .as_ref()?
        .init_container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == "docker")
}

/// Whether the pod's Docker sidecar restarted since `last_reported`
/// restarts, and if so its new restart count and the reason Kubernetes
/// gave for the last exit (e.g. `OOMKilled`).
pub(super) fn docker_restart_to_report(pod: &Pod, last_reported: i32) -> Option<(i32, Option<String>)> {
    let docker = docker_status(pod)?;
    if docker.restart_count <= last_reported {
        return None;
    }
    let reason = docker
        .last_state
        .as_ref()
        .and_then(|s| s.terminated.as_ref())
        .and_then(|t| t.reason.clone());
    Some((docker.restart_count, reason))
}

/// What `watch_pods` knows of a pod's Docker sidecar: the restart count
/// it last saw, and the reason of the last exit it saw (`state.terminated`
/// in the update before a restart; SME-85).
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct DockerSeen {
    pub(super) restarts: i32,
    pub(super) exit_reason: Option<String>,
}

/// A Docker sidecar restart for `watch_pods` to report.
#[derive(Debug, PartialEq)]
pub(super) enum DockerRestart {
    /// Report it now, with this reason.
    Report { pod_id: i64, reason: String },
    /// No update has said why yet: re-read the pod after
    /// `DOCKER_REASON_WAIT` and report it then.
    Recheck { pod_id: i64 },
}

/// `watch_pods`' bookkeeping for Docker sidecar restarts: remembers each
/// pod's restart count and last exit reason in `seen`, and returns a
/// restart to report. A pod first seen in a listing (`initial`, after
/// smelt starts or the watch reconnects) is only remembered: there's
/// nothing to compare its count with. A known pod's re-listing is compared
/// like any update.
pub(super) fn note_docker_restarts(
    seen: &mut HashMap<i64, DockerSeen>,
    pod: &Pod,
    initial: bool,
) -> Option<DockerRestart> {
    let pod_id = watched_pod_id(pod)?;
    // In a listing, a pod already known is one re-listed after the watch
    // reconnected: a restart since is one missed while disconnected (SME-88).
    let known = seen.contains_key(&pod_id);
    let entry = seen.entry(pod_id).or_default();
    let exited = docker_status(pod)
        .and_then(|c| c.state.as_ref()?.terminated.as_ref()?.reason.clone());
    if exited.is_some() {
        entry.exit_reason = exited;
    }
    let (count, reason) = docker_restart_to_report(pod, entry.restarts)?;
    entry.restarts = count;
    if initial && !known {
        // A starting point, not a restart: an exit it shows explains the
        // restart to come, so it's kept (SME-85's second code review).
        return None;
    }
    // A finished pod's containers are gone with it: crash cleanup reports
    // that, not a Docker restart (SME-88 code review).
    if pod_has_finished(pod) {
        return None;
    }
    // The exit seen before this restart explains only this one.
    let remembered = entry.exit_reason.take();
    Some(match reason.or(remembered) {
        Some(reason) => DockerRestart::Report { pod_id, reason },
        None => DockerRestart::Recheck { pod_id },
    })
}

/// Forgets the Docker sidecars of pods a re-listing didn't include: they
/// were deleted while the watch was disconnected, so no `Delete` came.
pub(super) fn forget_unlisted_docker(seen: &mut HashMap<i64, DockerSeen>, listed: &std::collections::HashSet<i64>) {
    seen.retain(|pod_id, _| listed.contains(pod_id));
}

/// How long `watch_pods` waits before re-reading a pod whose Docker
/// sidecar restarted with no reason given yet (SME-85).
pub(super) const DOCKER_REASON_WAIT: Duration = Duration::from_secs(3);

/// Reports `pod_id`'s Docker restart after `DOCKER_REASON_WAIT`, with the
/// reason the pod's status gives by then, if any.
pub(super) fn recheck_docker_restart(pool: PgPool, client: kube::Client, pod_id: i64) {
    tokio::spawn(async move {
        tokio::time::sleep(DOCKER_REASON_WAIT).await;
        let reason = match pods_api(&client).get_opt(&pod_name(pod_id)).await {
            Ok(Some(pod)) => docker_restart_to_report(&pod, 0).and_then(|(_, reason)| reason),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(pod_id, error = %e, "couldn't re-read a pod whose Docker restarted");
                None
            }
        };
        report_docker_restart(&pool, pod_id, reason).await;
    });
}

/// Reports `restart` now, or after a re-read when no update said why.
pub(super) async fn handle_docker_restart(pool: &PgPool, restart: DockerRestart) {
    match restart {
        DockerRestart::Report { pod_id, reason } => report_docker_restart(pool, pod_id, Some(reason)).await,
        DockerRestart::Recheck { pod_id } => recheck_docker_restart(pool.clone(), get().client.clone(), pod_id),
    }
}

/// Tells the pod's conversation that its Docker sidecar restarted, then
/// wakes it, the same way a crashed pod is reported (see
/// `handle_crash_cleanup`): the containers it ran are gone, which a
/// command waiting on one of them may need to hear about.
pub(super) async fn report_docker_restart(pool: &PgPool, pod_id: i64, reason: Option<String>) {
    let conversation_id = match db::sandbox_pod_conversation_id(pool, pod_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't find the conversation of a pod whose Docker restarted");
            return;
        }
    };
    let why = reason.map(|r| format!(" ({r})")).unwrap_or_default();
    let notice = format!(
        "Docker in sandbox pod {pod_id} was restarted{why}, so the containers it was running \
         have stopped. Images, terminals and /workspace are unaffected. Every container shares \
         Docker's memory limit; a pod created with a larger docker_memory_limit gives them more."
    );
    let pool = pool.clone();
    tokio::spawn(async move {
        crate::api::chat::deliver_notice(&pool, conversation_id, notice).await;
    });
}

/// Waits until no pod labelled with `conversation_id` is left in the
/// cluster, so a new pod never shares the Docker claim with one still
/// stopping.
pub(super) async fn wait_for_conversation_pods_gone(
    client: &kube::Client,
    conversation_id: i64,
    timeout: Duration,
) -> Result<(), SandboxError> {
    let pods = pods_api(client);
    let selector = ListParams::default().labels(&format!("{CONVERSATION_LABEL}={conversation_id}"));
    let waited = tokio::time::timeout(timeout, async {
        loop {
            let remaining = pods.list(&selector).await?;
            if remaining.items.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    waited.map_err(|_| {
        SandboxError::Timeout(Some(
            "the conversation's previous pod is still stopping".to_string(),
        ))
    })?
}

/// Creates the conversation's claims unless they already exist, so its
/// later pods reuse its images and build cache and find /workspace as the
/// last pod left it.
pub(super) async fn ensure_conversation_pvcs(client: &kube::Client, conversation_id: i64) -> Result<(), SandboxError> {
    let pvcs = pvc_api(client);
    for spec in conversation_pvc_specs(conversation_id) {
        let name = spec.metadata.name.clone().expect("named above");
        if pvcs.get_opt(&name).await?.is_some() {
            continue;
        }
        match pvcs.create(&PostParams::default(), &spec).await {
            // Another create for the same conversation got there first.
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            other => {
                other?;
            }
        }
    }
    Ok(())
}

/// Best-effort, like the rest of conversation teardown: logged, never
/// returned. The startup sweep catches whatever this misses.
pub(super) async fn delete_conversation_pvcs(client: &kube::Client, conversation_id: i64) {
    for name in [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)] {
        match pvc_api(client).delete(&name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => tracing::warn!(claim = %name, error = %e, "failed to delete a conversation's claim"),
        }
    }
}
