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

    match pvc_api(client).list(&unlabelled).await {
        Ok(claims) => {
            for claim in claims {
                adopt_if(&pvc_api(client), claim, &instance.id, decide).await;
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
        adopt_if(&pods, pod, &instance.id, decide).await;
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
        adopt_if(&pods, server, &instance.id, |meta| should_adopt_server(meta, &instance, &sandbox_pods)).await;
    }
}

/// Labels `object` with `instance` if `decide` says so; otherwise leaves
/// it, logged. The patch carries the object's `resourceVersion`, so one
/// changed since it was listed fails the patch instead of being
/// overwritten: it's read again and decided again, once.
async fn adopt_if<K>(api: &Api<K>, object: K, instance: &str, decide: impl Fn(&ObjectMeta) -> bool)
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let mut object = object;
    for attempt in 0..2 {
        let meta = object.meta();
        let Some(name) = meta.name.clone() else { return };
        if !decide(meta) {
            if attempt == 0 {
                tracing::info!(object = %name, "left an object with no smelt/instance label: not this database's to adopt");
            }
            return;
        }
        let patch = serde_json::json!({
            "metadata": {
                "labels": {INSTANCE_LABEL: instance},
                "resourceVersion": meta.resource_version,
            }
        });
        match api.patch(&name, &kube::api::PatchParams::default(), &kube::api::Patch::Merge(&patch)).await {
            Ok(_) => {
                tracing::info!(object = %name, "adopted an object made before SME-115");
                return;
            }
            Err(kube::Error::Api(e)) if e.code == 409 && attempt == 0 => match api.get_opt(&name).await {
                Ok(Some(fresh)) => object = fresh,
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!(object = %name, error = %e, "couldn't re-read an object to adopt");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(object = %name, error = %e, "couldn't adopt an object made before SME-115; retried at the next start");
                return;
            }
        }
    }
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
