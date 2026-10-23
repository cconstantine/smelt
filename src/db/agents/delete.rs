//! Deleting a conversation and its whole subtree (SME-236).

use super::*;

/// The subtree being deleted, as transaction A marked it.
#[derive(Debug, PartialEq)]
pub struct Deleting {
    /// The deleted conversation first, then everything under it in
    /// ascending id.
    pub subtree: Vec<i64>,
    /// The deleted conversation's parent, when it's an agent.
    pub parent: Option<i64>,
}

/// The delete's transaction A: the group's root row locked (the lock
/// `start_agent` takes, so the two exclude each other), then the subtree
/// marked `deleting_at` and its agents `stopped`. A `start_agent` that
/// committed first is in the subtree; one after this is refused. `None`
/// when the conversation is already gone.
pub async fn mark_subtree_deleting(pool: &PgPool, id: i64) -> Result<Option<Deleting>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let root = sqlx::query_scalar::<_, i64>(
        "WITH RECURSIVE up(id, parent_id) AS (
             SELECT id, parent_id FROM conversations WHERE id = $1
             UNION ALL
             SELECT c.id, c.parent_id FROM conversations c JOIN up ON c.id = up.parent_id
         )
         SELECT id FROM up WHERE parent_id IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(root) = root else {
        return Ok(None);
    };
    if !lock_conversations(&mut tx, &[(root, RowLock::NoKeyUpdate)]).await? {
        return Ok(None);
    }
    let Some(parent) = sqlx::query_scalar::<_, Option<i64>>("SELECT parent_id FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(None);
    };
    let subtree = subtree_ids(&mut tx, id).await?;
    sqlx::query("UPDATE conversations SET deleting_at = now() WHERE id = ANY($1)")
        .bind(&subtree)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE agents SET state = $2, state_changed_at = now()
         WHERE conversation_id = ANY($1) AND state IN ($3, $4, $5)",
    )
    .bind(&subtree)
    .bind(AGENT_STOPPED)
    .bind(AGENT_STARTING)
    .bind(AGENT_ACTIVE)
    .bind(AGENT_FINISHED)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(Deleting { subtree, parent }))
}

/// The delete's transaction B: the parent's row (shared) and the subtree's
/// rows (for update) locked in ascending id, the notice to the parent that
/// its agent was deleted (`label` names it), then the delete, whose cascade
/// removes the subtree. The notice lands only if the delete does.
pub async fn delete_subtree_rows(
    pool: &PgPool,
    id: i64,
    deleting: &Deleting,
    label: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut locks: Vec<(i64, RowLock)> = deleting.subtree.iter().map(|id| (*id, RowLock::Update)).collect();
    if let Some(parent) = deleting.parent {
        locks.push((parent, RowLock::KeyShare));
    }
    lock_conversations(&mut tx, &locks).await?;
    #[cfg(test)]
    crate::agents::test_hooks::reach(crate::agents::test_hooks::DELETE_LOCKED, id).await;
    if let Some(parent) = deleting.parent
        && sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM conversations WHERE id = $1)")
            .bind(parent)
            .fetch_one(&mut *tx)
            .await?
    {
        let body = format!("the user deleted {label}");
        queue_agent_message(&mut tx, parent, Some(id), label, KIND_NOTICE, &body).await?;
        #[cfg(test)]
        if crate::agents::test_hooks::fail_delete_after_notice(id) {
            return Err(sqlx::Error::Protocol("a test failed the delete after its notice".to_string()));
        }
    }
    sqlx::query("DELETE FROM conversations WHERE id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await
}
