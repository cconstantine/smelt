//! Adopting the cluster objects made before SME-115, which carry no
//! `smelt/instance` label: only the database that owns them (it already
//! had conversations or volumes when it migrated, `owns_unlabelled`)
//! labels them as its own, at startup, before the pod watch and the claim
//! sweep run. Nothing here ever deletes anything: an unlabelled object
//! that isn't adopted stays as it is, and is logged.

use super::*;

/// Whether this database adopts the unlabelled object `meta`: only with
/// `owns_unlabelled`, only an object with no instance label, and only one
/// whose conversation (by its `smelt/conversation` label) or volume (by
/// its `sandbox-volume-<id>` name) this database still has.
pub(super) fn should_adopt(
    meta: &ObjectMeta,
    instance: &db::SmeltInstance,
    conversations: &std::collections::HashSet<i64>,
    volumes: &std::collections::HashSet<i64>,
) -> bool {
    if !instance.owns_unlabelled || ownership(meta, &instance.id) != Ownership::Unlabelled {
        return false;
    }
    if let Some(conversation) = meta.labels.as_ref().and_then(|l| l.get(CONVERSATION_LABEL)) {
        return conversation.parse().is_ok_and(|id| conversations.contains(&id));
    }
    meta.name
        .as_deref()
        .and_then(|name| name.strip_prefix("sandbox-volume-"))
        .and_then(|id| id.parse().ok())
        .is_some_and(|id| volumes.contains(&id))
}

/// Whether this database adopts the unlabelled language server pod `meta`:
/// only one next to a sandbox pod that is now ours (`sandbox_pods`, by its
/// `smelt/lsp-pod` label).
pub(super) fn should_adopt_server(
    meta: &ObjectMeta,
    instance: &db::SmeltInstance,
    sandbox_pods: &std::collections::HashSet<i64>,
) -> bool {
    instance.owns_unlabelled
        && ownership(meta, &instance.id) == Ownership::Unlabelled
        && meta
            .labels
            .as_ref()
            .and_then(|l| l.get(crate::lsp::pods::LSP_POD_LABEL))
            .and_then(|id| id.parse().ok())
            .is_some_and(|id| sandbox_pods.contains(&id))
}

/// Whether this database adopts the unlabelled pod `meta`, named by one of
/// its pod records of `conversation_id`: only with `owns_unlabelled`, and
/// only one labelled with that conversation or with no conversation label
/// at all (a pod from before SME-33, SME-88), never another conversation's.
pub(super) fn should_adopt_record_pod(meta: &ObjectMeta, instance: &db::SmeltInstance, conversation_id: i64) -> bool {
    if !instance.owns_unlabelled || ownership(meta, &instance.id) != Ownership::Unlabelled {
        return false;
    }
    match meta.labels.as_ref().and_then(|l| l.get(CONVERSATION_LABEL)) {
        None => true,
        Some(label) => label.parse() == Ok(conversation_id),
    }
}

/// Adopts the unlabelled objects this database owns; see the module's doc.
pub async fn adopt_unlabelled_objects(pool: &PgPool) {
    match get() {
        Ok(manager) => adopt_unlabelled_objects_with(&manager.client, pool).await,
        Err(e) => tracing::warn!(error = %e, "couldn't adopt the cluster objects made before SME-115"),
    }
}

/// `adopt_unlabelled_objects` on `client`.
pub(super) async fn adopt_unlabelled_objects_with(client: &kube::Client, pool: &PgPool) {
    let instance = match db::smelt_instance(pool).await {
        Ok(instance) => instance,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read this database's instance; nothing adopted");
            return;
        }
    };
    if !instance.owns_unlabelled {
        // Every scratch database: said once, so a check server's log shows
        // adoption ran and touched nothing (SME-117).
        tracing::info!(instance = %instance.id, "this database owns no objects from before SME-115; nothing to adopt");
        return;
    }
    let conversations = match db::list_conversations(pool).await {
        Ok(list) => list.into_iter().map(|c| c.id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list conversations; nothing adopted");
            return;
        }
    };
    let volumes = match db::list_sandbox_volumes(pool).await {
        Ok(list) => list.into_iter().map(|v| v.id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list volumes; nothing adopted");
            return;
        }
    };
    let unlabelled = ListParams::default().labels(&format!("!{INSTANCE_LABEL}"));
    let decide = |meta: &ObjectMeta| should_adopt(meta, &instance, &conversations, &volumes);
    // What it did, for the summary at the end (SME-117).
    let mut tally = AdoptionTally::default();

    match pvc_api(client).list(&unlabelled).await {
        Ok(claims) => {
            for claim in claims {
                tally.count(adopt_if(&pvc_api(client), claim, &instance.id, decide).await);
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list unlabelled claims to adopt"),
    }

    let pods = pods_api(client);
    let listed = match pods.list(&unlabelled).await {
        Ok(list) => list.items,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list unlabelled pods to adopt");
            return;
        }
    };
    // Sandbox pods first: a language server pod goes with its sandbox pod.
    let (servers, others): (Vec<Pod>, Vec<Pod>) = listed.into_iter().partition(|pod| {
        pod.metadata.labels.as_ref().is_some_and(|l| l.contains_key(crate::lsp::pods::LSP_POD_LABEL))
    });
    for pod in others {
        tally.count(adopt_if(&pods, pod, &instance.id, decide).await);
    }
    let ours = ListParams::default().labels(&format!("{CONVERSATION_LABEL},{INSTANCE_LABEL}={}", instance.id));
    let sandbox_pods = match pods.list(&ours).await {
        Ok(list) => list.iter().filter_map(watched_pod_id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list our sandbox pods to adopt their language servers");
            return;
        }
    };
    for server in servers {
        tally.count(adopt_if(&pods, server, &instance.id, |meta| should_adopt_server(meta, &instance, &sandbox_pods)).await);
    }
    // A pod from before SME-33 has no conversation label; its live record
    // names it (SME-115 review 2).
    match db::list_live_pods(pool).await {
        Ok(rows) => {
            for row in rows {
                match pods.get_opt(&pod_name(row.pod_id)).await {
                    Ok(Some(pod)) if ownership(&pod.metadata, &instance.id) == Ownership::Unlabelled => tally.count(
                        adopt_if(&pods, pod, &instance.id, |meta| should_adopt_record_pod(meta, &instance, row.conversation_id))
                            .await,
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(pod_id = row.pod_id, error = %e, "couldn't read a live record's pod to adopt"),
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't list live pod records to adopt their pods"),
    }
    // Last, so its absence says adoption didn't finish.
    tracing::info!(
        instance = %instance.id,
        adopted = tally.adopted,
        left = tally.left,
        "adopted objects made before SME-115"
    );
}

/// How many objects startup's adoption labelled, and how many it left
/// unlabelled (not this database's, or a patch that failed).
#[derive(Default)]
struct AdoptionTally {
    adopted: usize,
    left: usize,
}

impl AdoptionTally {
    fn count(&mut self, adopted: bool) {
        if adopted {
            self.adopted += 1;
        } else {
            self.left += 1;
        }
    }
}

/// The merge patch that labels `meta`'s object with `instance`, held to
/// the object's identity: the API server refuses a patch carrying another
/// uid (422), so an object deleted and made again since it was read isn't
/// labelled. Not its `resourceVersion`: a pod's status changes several
/// times a second while it starts, which failed every try (SME-115
/// review 2). Adding one label overwrites nothing else.
pub(super) fn adoption_patch(meta: &ObjectMeta, instance: &str) -> serde_json::Value {
    serde_json::json!({
        "metadata": {
            "labels": {INSTANCE_LABEL: instance},
            "uid": meta.uid,
        }
    })
}

/// Labels `object` with `instance` if `decide` says so; otherwise leaves
/// it, logged. The patch is held to the object's uid (`adoption_patch`):
/// one replaced since it was listed fails it, and is read again and
/// decided again, once. True when it labelled it.
async fn adopt_if<K>(api: &Api<K>, object: K, instance: &str, decide: impl Fn(&ObjectMeta) -> bool) -> bool
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let mut object = object;
    for attempt in 0..2 {
        let meta = object.meta();
        let Some(name) = meta.name.clone() else { return false };
        let Some(uid) = meta.uid.as_deref() else {
            tracing::warn!(object = %name, "an object with no uid; not adopted");
            return false;
        };
        if !decide(meta) {
            if attempt == 0 {
                tracing::info!(object = %name, %uid, "left an object with no smelt/instance label: not this database's to adopt");
            }
            return false;
        }
        let patch = adoption_patch(meta, instance);
        // Read from the patch, which the call holds anyway: nothing more
        // is kept across its await (SME-115's future sizes).
        let patch_uid = |patch: &serde_json::Value| patch["metadata"]["uid"].as_str().unwrap_or_default().to_string();
        match api.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch)).await {
            Ok(_) => {
                tracing::info!(object = %name, uid = %patch_uid(&patch), "adopted an object made before SME-115");
                return true;
            }
            Err(kube::Error::Api(e)) if (e.code == 409 || e.code == 422) && attempt == 0 => match api.get_opt(&name).await {
                Ok(Some(fresh)) => object = fresh,
                Ok(None) => return false,
                Err(e) => {
                    tracing::warn!(object = %name, error = %e, "couldn't re-read an object to adopt");
                    return false;
                }
            },
            Err(e) => {
                tracing::warn!(object = %name, uid = %patch_uid(&patch), error = %e, "couldn't adopt an object made before SME-115; retried at the next start");
                return false;
            }
        }
    }
    false
}

/// Adopts one unlabelled object `decide` approves, for a bind point that
/// found it (startup's adoption may have missed it). True once it's ours.
pub(super) async fn adopt_one<K>(api: &Api<K>, object: K, instance: &str, decide: impl Fn(&ObjectMeta) -> bool) -> bool
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let Some(name) = object.meta().name.clone() else { return false };
    adopt_if(api, object, instance, decide).await;
    matches!(api.get_opt(&name).await, Ok(Some(now)) if ownership(now.meta(), instance) == Ownership::Ours)
}

/// Adopts `conversation_id`'s unlabelled sandbox pods and claims, for a
/// teardown by the database that owns them: startup's adoption may have
/// missed them, and the conversation's record is already gone, so the
/// next start never would. Language server pods go with their sandbox pod
/// (`ownerReferences`).
pub(super) async fn adopt_conversation_objects(
    client: &kube::Client,
    conversation_id: i64,
    pod_ids: &[i64],
    instance: &db::SmeltInstance,
) {
    let this_conversation = std::collections::HashSet::from([conversation_id]);
    let none = std::collections::HashSet::new();
    let decide = |meta: &ObjectMeta| should_adopt(meta, instance, &this_conversation, &none);
    let pods = pods_api(client);
    let selector = ListParams::default().labels(&format!("{CONVERSATION_LABEL}={conversation_id},!{INSTANCE_LABEL}"));
    match pods.list(&selector).await {
        Ok(list) => {
            for pod in list {
                adopt_if(&pods, pod, &instance.id, decide).await;
            }
        }
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't list a conversation's unlabelled pods to adopt"),
    }
    // And the pods its records name, with no conversation label (from
    // before SME-33; SME-115 review 2).
    for &pod_id in pod_ids {
        match pods.get_opt(&pod_name(pod_id)).await {
            Ok(Some(pod)) if ownership(&pod.metadata, &instance.id) == Ownership::Unlabelled => {
                adopt_if(&pods, pod, &instance.id, |meta| should_adopt_record_pod(meta, instance, conversation_id)).await;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(pod_id, error = %e, "couldn't read a record's pod to adopt"),
        }
    }
    let pvcs = pvc_api(client);
    for name in [docker_pvc_name(conversation_id), workspace_pvc_name(conversation_id)] {
        match pvcs.get_opt(&name).await {
            Ok(Some(claim)) if ownership(&claim.metadata, &instance.id) == Ownership::Unlabelled => {
                adopt_if(&pvcs, claim, &instance.id, decide).await;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(claim = %name, error = %e, "couldn't read a conversation's claim to adopt"),
        }
    }
}

/// What `adopt_record_pod` did.
#[derive(Debug, PartialEq)]
pub(super) enum RecordPodAdoption {
    /// It's now ours.
    Adopted,
    /// Not this record's to adopt: another conversation's, no record, or
    /// this database doesn't own objects from before SME-115.
    NotOurs,
    /// It should be ours, but labelling it failed (it's retried at the
    /// next start).
    Failed,
}

/// Adopts the unlabelled pod named by `pod_id`'s record, for the database
/// that owns objects from before SME-115 (`should_adopt_record_pod`).
pub(super) async fn adopt_record_pod(
    pool: &PgPool,
    pods: &Api<Pod>,
    pod_id: i64,
    instance: &db::SmeltInstance,
) -> RecordPodAdoption {
    let conversation_id = match db::sandbox_pod_conversation_id(pool, pod_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return RecordPodAdoption::NotOurs,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't read a pod record's conversation to adopt its pod");
            return RecordPodAdoption::Failed;
        }
    };
    let pod = match pods.get_opt(&pod_name(pod_id)).await {
        Ok(Some(pod)) => pod,
        Ok(None) => return RecordPodAdoption::NotOurs,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't read a pod to adopt");
            return RecordPodAdoption::Failed;
        }
    };
    if !should_adopt_record_pod(&pod.metadata, instance, conversation_id) {
        return RecordPodAdoption::NotOurs;
    }
    if adopt_one(pods, pod, &instance.id, |meta| should_adopt_record_pod(meta, instance, conversation_id)).await {
        RecordPodAdoption::Adopted
    } else {
        RecordPodAdoption::Failed
    }
}

/// What adoption logs at info (SME-117): every scratch database owns no
/// objects from before SME-115, and its log says so rather than nothing.
#[cfg(test)]
mod log_tests {
    use super::*;
    use crate::sandbox::claims::log_tests::{line_with, logged};
    use crate::sandbox::tests::test_client;

    #[sqlx::test]
    async fn test_adoption_on_a_fresh_database_says_there_is_nothing_to_adopt(pool: PgPool) {
        let client = test_client().await;
        let instance = db::smelt_instance(&pool).await.expect("instance").id;
        let ((), log) = logged(adopt_unlabelled_objects_with(&client, &pool)).await;
        assert!(
            line_with(&log, &["nothing to adopt", &format!("instance={instance}")]).is_some(),
            "adoption on a fresh database logged: {log}"
        );
    }

    /// The owning database's adoption ends with a summary. With no
    /// conversations, volumes or pod records nothing here is its to adopt,
    /// so it changes nothing in the shared namespace.
    #[sqlx::test]
    async fn test_adoption_by_the_owning_database_logs_a_summary(pool: PgPool) {
        let client = test_client().await;
        sqlx::query("UPDATE smelt_instance SET owns_unlabelled = true").execute(&pool).await.expect("an owning database");
        let instance = db::smelt_instance(&pool).await.expect("instance").id;
        let ((), log) = logged(adopt_unlabelled_objects_with(&client, &pool)).await;
        assert!(
            line_with(&log, &["adopted objects made before SME-115", &format!("instance={instance}"), "adopted=0", "left="])
                .is_some(),
            "the owning database's adoption logged: {log}"
        );
    }
}
