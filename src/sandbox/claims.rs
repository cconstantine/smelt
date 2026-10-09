//! A conversation's own claims (`/workspace`, Docker's data), their orphan
//! sweep, and Docker sidecar restarts.

use super::*;

/// A claim the startup sweep deletes: this database's, for a conversation
/// it no longer has.
#[derive(Debug, PartialEq)]
pub(super) struct OrphanedClaim {
    pub(super) conversation_id: i64,
    pub(super) name: String,
    /// Deleted only while it's still this object (SME-115).
    pub(super) uid: String,
}

/// The claims that outlived their conversation: labelled with a
/// conversation that isn't in `live`, and ours (SME-115). A claim from
/// before the fix (no instance label) or another database's is never an
/// orphan, whatever the selector that listed it.
pub(super) fn orphaned_docker_claims(
    claims: &[PersistentVolumeClaim],
    live: &std::collections::HashSet<i64>,
    instance: &str,
) -> Vec<OrphanedClaim> {
    let mut orphans: Vec<OrphanedClaim> = claims
        .iter()
        .filter(|c| ownership(&c.metadata, instance) == Ownership::Ours)
        // One already being deleted isn't deleted, logged and counted
        // again (SME-117 review 1).
        .filter(|c| c.metadata.deletion_timestamp.is_none())
        .filter_map(|c| {
            Some(OrphanedClaim {
                conversation_id: c.metadata.labels.as_ref()?.get(CONVERSATION_LABEL)?.parse().ok()?,
                name: c.metadata.name.clone()?,
                uid: c.metadata.uid.clone()?,
            })
        })
        .filter(|orphan| !live.contains(&orphan.conversation_id))
        .collect();
    orphans.sort_by(|a, b| a.name.cmp(&b.name));
    orphans
}

/// Deletes the claims of conversations this database no longer has.
/// Conversation teardown deletes its claims best-effort, so this catches
/// what that missed. Run once at startup by `main`.
pub async fn sweep_orphaned_conversation_claims(pool: &PgPool) {
    match get() {
        Ok(manager) => sweep_orphaned_conversation_claims_with(&manager.client, pool).await,
        Err(e) => tracing::warn!(error = %e, "couldn't sweep orphaned conversation claims"),
    }
}

/// `sweep_orphaned_conversation_claims` on `client`. Only this database's
/// claims (`smelt/instance`) are listed: every smelt server shares the
/// namespace, and to one whose database is empty (a scratch check server)
/// every other server's claims looked orphaned (SME-115).
pub(super) async fn sweep_orphaned_conversation_claims_with(client: &kube::Client, pool: &PgPool) {
    let instance = match db::smelt_instance(pool).await {
        Ok(instance) => instance.id,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read this database's instance; skipping the claim sweep");
            return;
        }
    };
    let selector = ListParams::default().labels(&format!("{CONVERSATION_LABEL},{INSTANCE_LABEL}={instance}"));
    // Claims first, then conversations: a claim only exists for a
    // conversation that already did, so none can be missed in between.
    let claims = match pvc_api(client).list(&selector).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list conversation claims to sweep");
            return;
        }
    };
    let live = match db::list_conversations(pool).await {
        Ok(conversations) => conversations.into_iter().map(|c| c.id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list conversations to sweep their claims");
            return;
        }
    };
    let listed = claims.len();
    let mut deleted = 0;
    for orphan in orphaned_docker_claims(&claims, &live, &instance) {
        tracing::info!(
            claim = %orphan.name,
            conversation_id = orphan.conversation_id,
            "deleting a deleted conversation's claim"
        );
        if delete_claim_if_unchanged(client, &orphan.name, &orphan.uid).await {
            deleted += 1;
        }
    }
    // Always, so a check server's log says the sweep ran and what it
    // touched, and its absence says it didn't finish (SME-117).
    tracing::info!(%instance, listed, deleted, "swept orphaned conversation claims");
}

/// Deletes claim `name` only while it's still the object with `uid`: one
/// deleted and made again since (by another server, say) is left alone.
/// Best-effort: logged, and true only when the delete was accepted.
pub(super) async fn delete_claim_if_unchanged(client: &kube::Client, name: &str, uid: &str) -> bool {
    let params = DeleteParams {
        preconditions: Some(kube::api::Preconditions {
            uid: Some(uid.to_string()),
            resource_version: None,
        }),
        ..Default::default()
    };
    match pvc_api(client).delete(name, &params).await {
        Ok(_) => {
            // The record of what this server deleted (SME-117).
            tracing::info!(claim = %name, %uid, "deleted claim");
            return true;
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {
            tracing::info!(claim = %name, "a claim was replaced since it was read; left alone")
        }
        Err(e) => tracing::warn!(claim = %name, error = %e, "failed to delete a claim"),
    }
    false
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
        DockerRestart::Recheck { pod_id } => match get() {
            Ok(manager) => recheck_docker_restart(pool.clone(), manager.client.clone(), pod_id),
            Err(e) => tracing::warn!(pod_id, error = %e, "couldn't re-read a Docker restart"),
        },
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
    crate::turn::notify(pool, conversation_id, vec![crate::turn::Notice::Deliver(notice)]);
}

/// Waits until no pod labelled with `conversation_id` is left in the
/// cluster, so a new pod never shares the Docker claim with one still
/// stopping. Ours, and ones with no instance label (from before SME-115:
/// possibly ours, and waiting harms nothing); never another database's.
pub(super) async fn wait_for_conversation_pods_gone(
    client: &kube::Client,
    conversation_id: i64,
    timeout: Duration,
    instance: &str,
) -> Result<(), SandboxError> {
    let pods = pods_api(client);
    let selectors = [
        format!("{CONVERSATION_LABEL}={conversation_id},{INSTANCE_LABEL}={instance}"),
        format!("{CONVERSATION_LABEL}={conversation_id},!{INSTANCE_LABEL}"),
    ]
    .map(|labels| ListParams::default().labels(&labels));
    let waited = tokio::time::timeout(timeout, async {
        loop {
            let mut remaining = false;
            for selector in &selectors {
                remaining |= !pods.list(selector).await?.items.is_empty();
            }
            if !remaining {
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
pub(super) async fn ensure_conversation_pvcs(
    client: &kube::Client,
    conversation_id: i64,
    instance: &db::SmeltInstance,
) -> Result<(), SandboxError> {
    let pvcs = pvc_api(client);
    for (name, spec) in conversation_pvc_specs(conversation_id, &instance.id) {
        // Boxed: each claim's reads, create and rare adoption hold two
        // claims across awaits, which inline doubled this function's
        // future in a debug build (SME-115 review 1).
        Box::pin(ensure_conversation_claim(&pvcs, name, spec, conversation_id, instance)).await?;
    }
    Ok(())
}

/// One of `ensure_conversation_pvcs`' claims: made if missing, and
/// refused unless it's ours.
async fn ensure_conversation_claim(
    pvcs: &Api<PersistentVolumeClaim>,
    name: String,
    spec: PersistentVolumeClaim,
    conversation_id: i64,
    instance: &db::SmeltInstance,
) -> Result<(), SandboxError> {
    let existing = match pvcs.get_opt(&name).await? {
        Some(claim) => claim,
        None => match pvcs.create(&PostParams::default(), &spec).await {
            Ok(_) => return Ok(()),
            // Another create got there first: whose is it?
            Err(kube::Error::Api(e)) if e.code == 409 => pvcs.get(&name).await?,
            Err(e) => return Err(e.into()),
        },
    };
    // Never another database's /workspace (SME-115). One from before the
    // fix that startup missed is adopted now, by its owner only.
    match ownership(&existing.metadata, &instance.id) {
        Ownership::Ours => Ok(()),
        Ownership::Unlabelled if instance.owns_unlabelled => {
            let this_conversation = std::collections::HashSet::from([conversation_id]);
            let none = std::collections::HashSet::new();
            let decide = |meta: &ObjectMeta| should_adopt(meta, instance, &this_conversation, &none);
            if adopt_one(pvcs, existing, &instance.id, decide).await {
                Ok(())
            } else {
                Err(SandboxError::NotOurs { name, ownership: Ownership::Unlabelled })
            }
        }
        other => Err(SandboxError::NotOurs { name, ownership: other }),
    }
}

/// Best-effort, like the rest of conversation teardown: logged, never
/// returned. The startup sweep catches whatever this misses. Only claims
/// labelled with `instance` are deleted (SME-115).
pub(super) async fn delete_conversation_pvcs(client: &kube::Client, conversation_id: i64, instance: &str) {
    for name in [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)] {
        // Only its ownership and uid are kept across the delete (see
        // `delete_terminated_pod`).
        let (owner, uid) = match pvc_api(client).get_opt(&name).await {
            // Already being deleted: nothing to do, or to log again
            // (SME-117 review 1).
            Ok(Some(claim)) if claim.metadata.deletion_timestamp.is_some() => continue,
            Ok(Some(claim)) => (ownership(&claim.metadata, instance), claim.metadata.uid),
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(claim = %name, error = %e, "couldn't read a conversation's claim to delete");
                continue;
            }
        };
        match (owner, uid) {
            (Ownership::Ours, Some(uid)) => {
                delete_claim_if_unchanged(client, &name, &uid).await;
            }
            (Ownership::Ours, None) => tracing::warn!(claim = %name, "a claim with no uid; left alone"),
            (Ownership::Unlabelled, _) => {
                tracing::warn!(claim = %name, "left a claim with no smelt/instance label (made before SME-115)")
            }
            (Ownership::Foreign, _) => {
                tracing::info!(claim = %name, "left another smelt database's claim of the same name")
            }
        }
    }
}

/// What startup and teardown log at info about what they delete (SME-117):
/// a check server's log is the only record of what its sweep and
/// teardowns did to the shared namespace. Real-cluster tests, in
/// `smelt-park-test`, each with ids no other test or run uses.
#[cfg(test)]
pub(super) mod log_tests {
    use super::*;
    use crate::sandbox::tests::test_client;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// `future`'s output, and what it logged at info and above, without
    /// colour, as a check server's log has it.
    pub(in crate::sandbox) async fn logged<T>(future: impl std::future::Future<Output = T>) -> (T, String) {
        use tracing::instrument::WithSubscriber;
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let output = future.with_subscriber(subscriber).await;
        let text = String::from_utf8(captured.0.lock().expect("log buffer").clone()).expect("utf-8 log");
        (output, text)
    }

    /// The line of `log` holding every one of `parts`, if any.
    pub(in crate::sandbox) fn line_with<'a>(log: &'a str, parts: &[&str]) -> Option<&'a str> {
        log.lines().find(|line| parts.iter().all(|part| line.contains(part)))
    }

    /// A conversation id no other test or run uses (as `tests.rs`'s).
    fn unused_conversation_id() -> i64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        3_000_000_000 + i64::try_from(nanos % 1_000_000_000).unwrap_or_default()
    }

    /// A small claim for `conversation_id` labelled with `instance`; its uid.
    async fn make_claim(client: &kube::Client, name: &str, conversation_id: i64, instance: &str) -> String {
        let mut spec = build_conversation_pvc_spec(name.to_string(), conversation_id, "1Mi".to_string(), "unused");
        spec.metadata.labels.get_or_insert_default().insert(INSTANCE_LABEL.to_string(), instance.to_string());
        let made = pvc_api(client).create(&PostParams::default(), &spec).await.expect("create a claim");
        made.metadata.uid.expect("a new claim's uid")
    }

    /// A pod that never starts (an image that doesn't exist), labelled
    /// with `labels`; its uid.
    async fn make_pending_pod(client: &kube::Client, name: &str, labels: serde_json::Value) -> String {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "labels": labels},
            "spec": {"containers": [{"name": "sandbox", "image": "smelt.invalid/none:0"}]},
        }))
        .expect("pod");
        let made = pods_api(client).create(&PostParams::default(), &pod).await.expect("create pod");
        made.metadata.uid.expect("a new pod's uid")
    }

    /// The sweep always says what it did, even when it found nothing, and
    /// names each claim it deleted with its uid.
    #[sqlx::test]
    async fn test_the_claim_sweep_logs_a_summary_and_each_claim_it_deletes(pool: PgPool) {
        let client = test_client().await;
        let ours = db::smelt_instance(&pool).await.expect("instance").id;
        let ((), nothing) = logged(sweep_orphaned_conversation_claims_with(&client, &pool)).await;

        let conversation_id = unused_conversation_id();
        let orphan = workspace_pvc_name(conversation_id);
        let uid = make_claim(&client, &orphan, conversation_id, &ours).await;
        let ((), swept) = logged(sweep_orphaned_conversation_claims_with(&client, &pool)).await;
        pvc_api(&client).delete(&orphan, &DeleteParams::default()).await.ok();

        let instance = format!("instance={ours}");
        assert!(
            line_with(&nothing, &["swept orphaned conversation claims", &instance, "listed=0", "deleted=0"]).is_some(),
            "a sweep with nothing to do logged: {nothing}"
        );
        assert!(
            line_with(&swept, &["swept orphaned conversation claims", &instance, "listed=1", "deleted=1"]).is_some(),
            "the sweep's summary: {swept}"
        );
        assert!(
            line_with(&swept, &["deleted claim", &format!("claim={orphan}"), &format!("uid={uid}")]).is_some(),
            "the sweep didn't log the claim it deleted: {swept}"
        );
    }

    /// A conversation's teardown says it started, then names each pod and
    /// claim it deleted with its uid. Another database's claim of the same
    /// name is only "left".
    #[tokio::test]
    async fn test_a_conversations_teardown_logs_what_it_deletes() {
        let client = test_client().await;
        let conversation_id = unused_conversation_id();
        let pod = pod_name(conversation_id);
        let pod_uid = make_pending_pod(
            &client,
            &pod,
            serde_json::json!({CONVERSATION_LABEL: conversation_id.to_string(), INSTANCE_LABEL: TEST_INSTANCE}),
        )
        .await;
        let claims = [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)];
        let mut claim_uids = Vec::new();
        for claim in &claims {
            claim_uids.push(make_claim(&client, claim, conversation_id, TEST_INSTANCE).await);
        }
        // Another database's conversation of the next id.
        let theirs_id = conversation_id + 1;
        let theirs = workspace_pvc_name(theirs_id);
        make_claim(&client, &theirs, theirs_id, "another-smelt-database").await;

        let ((), log) = logged(teardown_conversation_with(&client, conversation_id, &[], &test_instance())).await;
        let ((), their_log) = logged(teardown_conversation_with(&client, theirs_id, &[], &test_instance())).await;

        pods_api(&client).delete(&pod, &immediate_delete_params()).await.ok();
        for claim in claims.iter().chain([&theirs]) {
            pvc_api(&client).delete(claim, &DeleteParams::default()).await.ok();
        }
        assert!(
            line_with(
                &log,
                &["tearing down a conversation's sandbox", &format!("conversation_id={conversation_id}"), &format!("instance={TEST_INSTANCE}")]
            )
            .is_some(),
            "no opening line: {log}"
        );
        assert!(
            line_with(&log, &["deleted pod", &format!("pod={pod}"), &format!("uid={pod_uid}")]).is_some(),
            "the pod's delete wasn't logged: {log}"
        );
        for (claim, uid) in claims.iter().zip(&claim_uids) {
            assert!(
                line_with(&log, &["deleted claim", &format!("claim={claim}"), &format!("uid={uid}")]).is_some(),
                "{claim}'s delete wasn't logged: {log}"
            );
        }
        assert!(
            line_with(&their_log, &["left another smelt database's claim", &format!("claim={theirs}")]).is_some(),
            "another database's claim: {their_log}"
        );
        assert!(!their_log.contains("deleted claim"), "another database's claim logged as deleted: {their_log}");
    }

    /// A pod a teardown deletes by its label and again by its record's id
    /// is logged as deleted once: the second finds it terminating and
    /// leaves it (SME-117 review 1).
    #[tokio::test]
    async fn test_a_teardown_logs_a_pod_its_record_also_names_once() {
        let client = test_client().await;
        let conversation_id = unused_conversation_id();
        let pod = pod_name(conversation_id);
        let pod_uid = make_pending_pod(
            &client,
            &pod,
            serde_json::json!({CONVERSATION_LABEL: conversation_id.to_string(), INSTANCE_LABEL: TEST_INSTANCE}),
        )
        .await;

        let ((), log) =
            logged(teardown_conversation_with(&client, conversation_id, &[conversation_id], &test_instance())).await;

        pods_api(&client).delete(&pod, &immediate_delete_params()).await.ok();
        let deleted = log.lines().filter(|line| line.contains("deleted pod") && line.contains(&format!("uid={pod_uid}"))).count();
        assert_eq!(deleted, 1, "the pod's delete was logged {deleted} times: {log}");
    }

    /// A claim already being deleted isn't an orphan to delete again: the
    /// sweep would log and count it a second time (SME-117 review 1).
    #[test]
    fn test_a_terminating_claim_is_not_swept_again() {
        let mut claim = PersistentVolumeClaim::default();
        claim.metadata.name = Some("sandbox-workspace-5".to_string());
        claim.metadata.uid = Some("uid-5".to_string());
        claim.metadata.labels = Some(
            [(CONVERSATION_LABEL.to_string(), "5".to_string()), (INSTANCE_LABEL.to_string(), TEST_INSTANCE.to_string())]
                .into_iter()
                .collect(),
        );
        let live = std::collections::HashSet::new();
        assert_eq!(orphaned_docker_claims(std::slice::from_ref(&claim), &live, TEST_INSTANCE).len(), 1);
        claim.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(k8s_openapi::jiff::Timestamp::now()));
        assert!(orphaned_docker_claims(&[claim], &live, TEST_INSTANCE).is_empty());
    }

    /// Stopping a pod logs its delete with its uid.
    #[sqlx::test]
    async fn test_a_terminated_pods_delete_is_logged(pool: PgPool) {
        let client = test_client().await;
        db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
        let ours = db::smelt_instance(&pool).await.expect("instance").id;
        let conversation = db::create_conversation(&pool).await.expect("conversation");
        let row = db::create_sandbox_pod(&pool, conversation.id).await.expect("pod row");
        let name = pod_name(row.id);
        let uid = make_pending_pod(
            &client,
            &name,
            serde_json::json!({CONVERSATION_LABEL: conversation.id.to_string(), INSTANCE_LABEL: ours}),
        )
        .await;

        let (closed, log) = logged(force_terminate_pod_with(&pool, &client, row.id)).await;

        pods_api(&client).delete(&name, &immediate_delete_params()).await.ok();
        assert!(closed.is_ok(), "{closed:?}");
        assert!(
            line_with(&log, &["deleted pod", &format!("pod={name}"), &format!("uid={uid}")]).is_some(),
            "the terminated pod's delete wasn't logged: {log}"
        );
    }

    /// Deleting a volume logs its claim's delete with its uid.
    #[tokio::test]
    async fn test_a_volume_claims_delete_is_logged() {
        let client = test_client().await;
        let id = unused_conversation_id();
        let name = sandbox_volume_pvc_name(id);
        let made = pvc_api(&client)
            .create(&PostParams::default(), &build_volume_pvc_spec(id, TEST_INSTANCE))
            .await
            .expect("create the claim");
        let uid = made.metadata.uid.expect("a new claim's uid");

        let (deleted, log) = logged(delete_volume_claim(&client, id, &test_instance())).await;

        pvc_api(&client).delete(&name, &DeleteParams::default()).await.ok();
        assert!(deleted.is_ok(), "{deleted:?}");
        assert!(
            line_with(&log, &["deleted volume claim", &format!("claim={name}"), &format!("uid={uid}")]).is_some(),
            "the volume claim's delete wasn't logged: {log}"
        );
    }
}
