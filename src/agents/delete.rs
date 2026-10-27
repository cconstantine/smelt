//! Deleting a conversation deletes its whole subtree (SME-234's question
//! 3): its agents, their agents, and each one's turns, browsing session
//! and pods.

use super::*;

/// Deletes `id` and every conversation under it. Marks the subtree first
/// (`deleting_at`, its agents `stopped`), so no `start_agent` adds to it;
/// stops every turn and browsing session in it; deletes the rows, telling
/// the deleted agent's parent in the same transaction; then tears down
/// each one's pods and forgets its turn state and event channel.
pub(crate) async fn delete_subtree(pool: &PgPool, id: i64) -> Result<(), String> {
    let Some(deleting) = db::mark_subtree_deleting(pool, id).await.map_err(|e| e.to_string())? else {
        // Already gone: forget what's left, as a delete would.
        crate::events::forget(id);
        crate::turn::forget_conversation_lock(id);
        return Ok(());
    };
    #[cfg(test)]
    test_hooks::reach(test_hooks::DELETE_MARKED, id).await;
    // Its turns first, so nothing is still making a pod (SME-51 B5).
    let mut pods = Vec::with_capacity(deleting.subtree.len());
    for conversation_id in &deleting.subtree {
        crate::turn::stop_turn_now(*conversation_id);
        let _ = crate::browsing::close_session(*conversation_id).await;
        // Read before the delete cascades them away: teardown deletes the
        // pods they name too, labelled or not (SME-88). Best-effort.
        let pod_ids: Vec<i64> = match db::list_sandbox_pods(pool, *conversation_id).await {
            Ok(rows) => rows.into_iter().map(|row| row.id).collect(),
            Err(e) => {
                tracing::warn!(conversation_id, error = %e, "couldn't list a conversation's pods before deleting it");
                Vec::new()
            }
        };
        pods.push((*conversation_id, pod_ids));
    }
    let label = label_of(pool, id).await;
    // The volumes to take with the subtree, read before the cascade (SME-239):
    // only what a conversation in it *created*, since a conversation that
    // merely mounts one loses its mount row and the volume stays.
    let volume_ids: Vec<i64> = match db::agent_volume_ids_of(pool, &deleting.subtree).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(conversation_id = id, error = %e, "couldn't list the subtree's volumes before deleting it; its claims are left for the startup sweep");
            Vec::new()
        }
    };
    db::delete_subtree_rows(pool, id, &deleting, &label).await.map_err(|e| e.to_string())?;
    if let Some(parent) = deleting.parent {
        crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
    }
    // After the delete: a pod started meanwhile is found by its label, and
    // one still starting sees the conversation gone (`create_pod`).
    for (conversation_id, pod_ids) in pods {
        #[cfg(test)]
        test_hooks::torn_down(conversation_id, &pod_ids);
        crate::sandbox::teardown_conversation(pool, conversation_id, &pod_ids).await;
        crate::events::forget(conversation_id);
        crate::turn::forget_conversation_lock(conversation_id);
    }
    // And the volume claims, once the pods that mounted them are gone
    // (SME-239): Kubernetes' `pvc-protection` holds a claim until its last
    // mounting pod ends, and the teardown above is what ends those. Best-
    // effort either way — a crash here leaves an orphan claim, which is the
    // startup sweep's one job.
    if !volume_ids.is_empty() {
        match crate::sandbox::get() {
            Ok(manager) => match db::smelt_instance(pool).await {
                Ok(instance) => {
                    for volume_id in volume_ids {
                        if let Err(e) = crate::sandbox::delete_agent_volume_claim(manager, volume_id, &instance).await {
                            tracing::warn!(volume = volume_id, error = %e, "the volume's rows are gone; its claim waits for the startup sweep");
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    conversation_id = id,
                    error = %e,
                    "couldn't read this instance for a deleted subtree's volume claims; the startup sweep takes them"
                ),
            },
            Err(e) => tracing::warn!(
                conversation_id = id,
                error = %e,
                "no sandbox manager: a deleted subtree's volume claims wait for the startup sweep"
            ),
        }
    }
    Ok(())
}
