use super::*;

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
/// In a `teardown_conversation` span (SME-137).
pub async fn teardown_conversation(pool: &PgPool, conversation_id: i64, pod_ids: &[i64]) {
    crate::telemetry::in_span(
        tracing::info_span!("teardown_conversation", conversation_id),
        teardown_conversation_unspanned(pool, conversation_id, pod_ids),
    )
    .await
}

async fn teardown_conversation_unspanned(pool: &PgPool, conversation_id: i64, pod_ids: &[i64]) {
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
pub(in crate::sandbox) async fn teardown_conversation_with(
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

pub(in crate::sandbox) async fn delete_listed(pods: &Api<Pod>, selector: &ListParams, conversation_id: i64, instance: &str) {
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
pub(in crate::sandbox) async fn delete_pod_if_ours(pods: &Api<Pod>, name: &str, instance: &str) {
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
pub(in crate::sandbox) async fn delete_pod_if_unchanged(pods: &Api<Pod>, name: &str, uid: &str) {
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
