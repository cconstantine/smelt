use super::*;

/// Creates any volume claim that's missing from this namespace. Every pod
/// mounts every configured volume, and a pod whose claim doesn't exist
/// can never be scheduled — it just waits until the start-up timeout. A
/// claim goes missing when the cluster is rebuilt or the claim deleted,
/// or when the volume was created in another namespace (the browser tier
/// shares the dev database but runs in the test namespace). Whatever the
/// old claim held is gone either way; recreating it keeps sandboxes working.
pub(in crate::sandbox) async fn ensure_volume_claims(
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
pub(in crate::sandbox) async fn delete_volume_claim(
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

/// Creates any claim this conversation's pod mounts that's missing, the same
/// shape as `ensure_volume_claims` for a global volume: a pod whose claim
/// doesn't exist can only wait out the start-up timeout, and a claim goes
/// missing when the cluster is rebuilt, or when the row was committed and the
/// claim create wasn't (SME-239's `create_volume` has that gap by design —
/// the row is the record, the claim is re-made here).
pub(in crate::sandbox) async fn ensure_agent_volume_claims(
    client: &kube::Client,
    volumes: &[db::AgentVolume],
    instance: &db::SmeltInstance,
) -> Result<(), SandboxError> {
    let pvcs = pvc_api(client);
    for volume in volumes {
        // Boxed, like the other claim ensures (SME-115 review 1).
        Box::pin(ensure_agent_volume_claim(&pvcs, volume, instance)).await?;
    }
    Ok(())
}

/// One of `ensure_agent_volume_claims`' claims: made if missing, and refused
/// unless it's ours. No adoption branch — an agent-volume claim postdates
/// SME-115, so an unlabelled one of that name is somebody else's and the start
/// fails closed, as for a foreign global-volume claim.
async fn ensure_agent_volume_claim(
    pvcs: &Api<PersistentVolumeClaim>,
    volume: &db::AgentVolume,
    instance: &db::SmeltInstance,
) -> Result<(), SandboxError> {
    let claim = agent_volume_pvc_name(volume.id);
    match pvcs.get_opt(&claim).await? {
        None => {
            tracing::warn!(claim = %claim, "volume claim missing; recreating it");
            pvcs.create(
                &PostParams::default(),
                &build_agent_volume_pvc_spec(volume.id, volume.owner_conversation_id, &instance.id),
            )
            .await?;
            Ok(())
        }
        Some(existing) => match ownership(&existing.metadata, &instance.id) {
            Ownership::Ours => Ok(()),
            other => Err(SandboxError::NotOurs { name: claim, ownership: other }),
        },
    }
}
