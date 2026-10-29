use super::*;
pub use crate::api::pods::StorageRemoval;

/// How long a removal waits for the conversation's pods — its sandbox
/// pod and its language-server pods — to be gone before giving up, so
/// the claims' `pvc-protection` finalizers release. Outlasts
/// `POD_DELETE_GRACE_SECS` (20 s) plus scheduling slack.
const STORAGE_REMOVAL_POD_WAIT: Duration = Duration::from_secs(30);

/// The error left on `ready` repo rows whose checkouts died with the
/// storage: the panel shows "Clone failed" and `clone_repo`'s retry
/// path works from there.
const STORAGE_REMOVED_ERROR: &str = "the sandbox storage was deleted; clone the repo again";

/// Deletes a conversation's two sandbox claims — its /workspace and its
/// Docker data — leaving the conversation and its transcript alone
/// (SME-253). Stops what mounts them first: its running pod, then its
/// language-server pods, since only after their last holder is gone do
/// the claims' finalizers release.
///
/// Takes the conversation's pod-start lock for its whole body (SME-253
/// review 1, finding 1): a start racing a removal is then either wholly
/// before it — its pod row is live and stopped below — or queued behind
/// the lock and meets the reuse guard normally (its own SME-122 re-check
/// reads its row after ours closed, and its claim bind waits out or
/// skips what this removal left Terminating). Nothing below takes any
/// other lock, so the bounded pod wait inside cannot deadlock.
pub async fn remove_conversation_storage(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<StorageRemoval, SandboxError> {
    let manager = get()?;
    // One removal or one start at a time for this conversation, for the
    // whole body (review 1, finding 1).
    let start_lock = pod_start_lock(conversation_id);
    let _starting = start_lock.lock().await;
    // Gone: its teardown owns its claims now, and a panel mid-delete
    // shouldn't error.
    if !db::conversation_exists(pool, conversation_id).await.map_err(SandboxError::Db)? {
        return Ok(StorageRemoval::default());
    }
    // Without it nothing can be told ours (SME-115): abort with a logged
    // warning, teardown's posture.
    let instance = match db::smelt_instance(pool).await {
        Ok(instance) => instance,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't read this database's instance; its conversation's storage is left");
            return Err(SandboxError::Db(e));
        }
    };
    tracing::info!(conversation_id, instance = %instance.id, "removing a conversation's sandbox storage");
    let mut removal = StorageRemoval::default();

    // Its live pods first. The same un-noticing mechanics a user Stop
    // uses — then `force_terminate_pod`, which publishes the
    // `SandboxPodUpdate { terminated }` and `PodsChanged` itself.
    // `stop_pod_for_user` is deliberately not called: its "user stopped
    // pod" notice would double with the removal notice below.
    for row in db::list_sandbox_pods(pool, conversation_id).await.map_err(SandboxError::Db)? {
        close_pod_terminals(pool, row.id).await;
        force_terminate_pod(pool, row.id).await?;
        removal.pod_stopped = true;
    }
    // Its language-server pods mount the workspace claim too; their
    // stdio connections drop and restart lazily, as they already do
    // when teardown deletes them.
    let lsp_selector = ListParams::default().labels(&format!(
        "{}={conversation_id},{INSTANCE_LABEL}={}",
        crate::lsp::pods::LSP_OF_LABEL,
        instance.id
    ));
    let pods = pods_api(&manager.client);
    removal.lsp_pods_stopped = match pods.list(&lsp_selector).await {
        Ok(list) => u8::try_from(
            list.into_iter()
                .filter(|pod| {
                    ownership(&pod.metadata, &instance.id) == Ownership::Ours
                        && pod.metadata.deletion_timestamp.is_none()
                })
                .count(),
        )
        .unwrap_or(u8::MAX),
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't list the conversation's language-server pods");
            0
        }
    };
    delete_listed(&pods, &lsp_selector, conversation_id, &instance.id).await;

    // Only once no holder mounts them do the deletions below finish.
    if let Err(e) = wait_for_conversation_pods_gone(
        &manager.client,
        conversation_id,
        STORAGE_REMOVAL_POD_WAIT,
        &instance.id,
        &[crate::lsp::pods::LSP_OF_LABEL],
    )
    .await
    {
        // The pod row is already closed above; the claims are intact and
        // a retry gets here again (the wait re-checks).
        tracing::warn!(conversation_id, error = ?e, "the conversation's pods outlasted the storage removal's wait");
        return Err(SandboxError::Timeout(Some(
            "the conversation's pods are still stopping; try again".to_string(),
        )));
    }

    // Both claims, by name — never by label selector, so 239's agent
    // volumes and the global sandbox volumes claims can never be caught
    // even if labels change later.
    let mut deleted: Vec<String> = Vec::new();
    // The checkouts live on the workspace claim: if that one is left
    // behind (Failed or Foreign), its disk and everything cloned on it
    // are still there, and neither the repos nor the model are told
    // otherwise (SME-253 review 1, finding 2). Already gone counts as
    // gone; Foreign means it stands, another server's.
    let mut workspace_gone = true;
    for (name, is_workspace) in [(docker_pvc_name(conversation_id), false), (workspace_pvc_name(conversation_id), true)] {
        match remove_conversation_claim(&manager.client, &name, &instance.id).await {
            ClaimRemoval::Deleted => {
                removal.claims_deleted += 1;
                deleted.push(name);
            }
            ClaimRemoval::Failed => {
                removal.claims_failed += 1;
                if is_workspace {
                    workspace_gone = false;
                }
            }
            ClaimRemoval::AlreadyGone => {}
            ClaimRemoval::Foreign => {
                if is_workspace {
                    workspace_gone = false;
                }
            }
        }
    }

    // A few seconds' bounded confirm of the names just deleted: this is
    // what lands the panel's post-remove refetch on "none" rather than
    // "removing…", and what lets the very next `create_pod` skip the
    // reuse guard's wait. Best-effort — the guard covers whoever still
    // lands in the window.
    if !deleted.is_empty() {
        let confirm = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut left = 0;
                for name in &deleted {
                    if pvc_api(&manager.client).get_opt(name).await.ok().flatten().is_some() {
                        left += 1;
                    }
                }
                if left == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        if confirm.is_err() {
            tracing::info!(conversation_id, "a claim was still deleting as the removal returned");
            // The chat panel refetches its stored volumes on
            // `PodsChanged` and on nothing else (the /pods page has a
            // 30 s tick; the panel doesn't), and this call's event
            // published while the claim still stood — without another
            // word its row sits at "removing…" until some unrelated
            // event (SME-253 review 1, finding 3). Announce once more
            // when the names themselves are gone; bounded, so a stuck
            // finalizer costs one quiet task and not a leak, and its
            // late event is just a refetch of what already shows.
            let client = manager.client.clone();
            let names = deleted;
            tokio::spawn(async move {
                let gone_by = tokio::time::Instant::now() + Duration::from_secs(600);
                loop {
                    let mut left = 0;
                    for name in &names {
                        if pvc_api(&client).get_opt(name).await.ok().flatten().is_some() {
                            left += 1;
                        }
                    }
                    if left == 0 || tokio::time::Instant::now() >= gone_by {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                events::publish_app(events::AppEvent::PodsChanged);
            });
        }
    }

    // The checkouts are gone with the workspace claim: mark the
    // conversation's finished repos failed — unless that claim stood,
    // in which case they're all still on disk (review 1, finding 2).
    // Logged, not returned: a display lag with nothing on disk depending
    // on it (the panel refreshes with the repos themselves).
    if workspace_gone {
        match db::fail_repos_after_storage_removal(pool, conversation_id, STORAGE_REMOVED_ERROR).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(conversation_id, repos_failed = n, "marked the conversation's repos after a storage removal"),
            Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't mark the conversation's repos after a storage removal"),
        }
    }

    // The model learns on its next turn, like a user Stop — and only
    // when something was running to experience it, and it really lost
    // its storage (review 1, finding 2: a claim left standing keeps its
    // files; the model must not be told to re-clone over live ones).
    if removal.pod_stopped && workspace_gone {
        crate::turn::notify(
            pool,
            conversation_id,
            vec![crate::turn::Notice::Save(
                "The user removed this conversation's sandbox storage: /workspace and the Docker images, build cache and volumes are gone. Repos there need cloning again; the next sandbox starts with fresh empty volumes."
                    .to_string(),
            )],
        );
    }
    // Reuse the pods event rather than add an `AppEvent` variant: an
    // unknown variant would trip an older tab's stream loop
    // (`frontend/pages/pods.rs`). Every non-empty case says so.
    if removal != StorageRemoval::default() {
        events::publish_app(events::AppEvent::PodsChanged);
    }
    // The record of what this server deleted (SME-117).
    tracing::info!(conversation_id, ?removal, "removed a conversation's sandbox storage");
    Ok(removal)
}
