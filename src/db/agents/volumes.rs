//! The rows behind a model-created volume (SME-239): the `agent_volumes` row
//! and the `conversation_volume_mounts` rows that say whose pod mounts it.
//! The shape is the design's, and it stays as the design settles it
//! ([checkouts-and-volumes](../../../docs/design/personas/checkouts-and-volumes.md#volumes)):
//! one row per volume, owned by the conversation that created it, and a mount
//! row per conversation that mounts it — the owner first, then every child it
//! was passed to. `ON DELETE CASCADE` does the deleting; the startup sweep
//! (`crate::sandbox`) catches a claim its cut-off delete left behind.
//!
//! Lock order, as this module's parent states it: `conversations` rows first,
//! then `agent_volumes` rows. `create_child` takes the parent's conversation
//! row `FOR NO KEY UPDATE` and then the volume rows `FOR SHARE`;
//! `delete_volume` takes one volume row `FOR UPDATE` and nothing above it; the
//! subtree cascade goes conversations → agent_volumes. One order every way, so
//! no cycle.

use super::*;

/// One model-created volume, as a pod spec and the tools need it. The mount
/// path is stored normalized and resolved (`~/d` is saved as
/// `/home/sandbox/d`): what the pod mounts and what the prompt lists are then
/// the same string.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct AgentVolume {
    pub id: i64,
    pub owner_conversation_id: i64,
    pub name: String,
    pub mount_path: String,
}

/// Every volume this conversation's pod mounts: the ones it created and the
/// ones a parent passed down. Read once per pod start, and the prompt re-grows
/// it per turn — neither caches, and Kubernetes can only mount at the next
/// start (which is what the tools' result says when the pod is live).
pub async fn list_conversation_volume_mounts(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<AgentVolume>, sqlx::Error> {
    sqlx::query_as::<_, AgentVolume>(
        "SELECT v.id, v.owner_conversation_id, v.name, v.mount_path
           FROM conversation_volume_mounts m
           JOIN agent_volumes v ON v.id = m.volume_id
          WHERE m.conversation_id = $1
          ORDER BY v.id",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// The volume named `name` that `conversation_id` mounts — one it created or
/// one it was passed to. `create_volume` refuses a name this answers with, so
/// a `start_agent(volumes: […])` name never means two volumes (the plan's
/// first Decision), and `delete_volume` reads the same row to name whoever
/// created the volume it was asked about.
pub async fn agent_volume_held_by(
    pool: &PgPool,
    conversation_id: i64,
    name: &str,
) -> Result<Option<AgentVolume>, sqlx::Error> {
    sqlx::query_as::<_, AgentVolume>(
        "SELECT v.id, v.owner_conversation_id, v.name, v.mount_path
           FROM agent_volumes v
           JOIN conversation_volume_mounts m ON m.volume_id = v.id
          WHERE m.conversation_id = $1 AND v.name = $2",
    )
    .bind(conversation_id)
    .bind(name)
    .fetch_optional(pool)
    .await
}

/// Whether this conversation already mounts anything at `mount_path`. A pod
/// mounts one volume per path, so a second claim at the same path would
/// quietly shadow the first — which is why the tool refuses it rather than
/// letting the pod decide.
pub async fn volume_mounted_at(
    pool: &PgPool,
    conversation_id: i64,
    mount_path: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1 FROM conversation_volume_mounts m
              JOIN agent_volumes v ON v.id = m.volume_id
             WHERE m.conversation_id = $1 AND v.mount_path = $2)",
    )
    .bind(conversation_id)
    .bind(mount_path)
    .fetch_one(pool)
    .await
}

/// The volume row and its owner's mount row, in one transaction: the
/// conversation that creates a volume mounts it too, so it gets a mount row
/// exactly like every child it later passes it to (the design's schema).
pub async fn create_agent_volume_with_owner_mount(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    name: &str,
    mount_path: &str,
) -> Result<AgentVolume, sqlx::Error> {
    let volume = sqlx::query_as::<_, AgentVolume>(
        "INSERT INTO agent_volumes (owner_conversation_id, name, mount_path)
         VALUES ($1, $2, $3)
         RETURNING id, owner_conversation_id, name, mount_path",
    )
    .bind(conversation_id)
    .bind(name)
    .bind(mount_path)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
        .bind(conversation_id)
        .bind(volume.id)
        .execute(&mut *conn)
        .await?;
    Ok(volume)
}

/// One volume row, by id and creator, taken back: `create_volume`'s rollback
/// when the claim create fails after the commit, the shape the user's own
/// volumes page uses. Its mount rows cascade with it.
pub async fn delete_agent_volume_row(
    pool: &PgPool,
    owner_conversation_id: i64,
    volume_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM agent_volumes WHERE id = $1 AND owner_conversation_id = $2")
        .bind(volume_id)
        .bind(owner_conversation_id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

/// The volumes named in `names` that `conversation_id` holds — one it created
/// or one passed down to it — each locked `FOR SHARE` for as long as the
/// transaction that records a child's mount rows runs (SME-239).
/// `create_child` runs it, and that share is the pair with
/// [`delete_agent_volume_if_unused`]'s `FOR UPDATE`: a delete either waits for
/// the child's rows to commit and then refuses because the child it just
/// mounted is `starting`, or it commits first and this lookup answers with no
/// row for that name, and the start refuses. Either way one of the two
/// refuses, and no child is left mounted to a volume that isn't there.
///
/// A name the conversation doesn't hold is simply absent — `create_child`
/// names the first missing one in its refusal. `names` may repeat: the join
/// answers once per volume, and mount rows are keyed by (conversation,
/// volume), so a repeated name still means one row.
pub async fn agent_volumes_held_for(
    conn: &mut sqlx::PgConnection,
    conversation_id: i64,
    names: &[&str],
) -> Result<Vec<AgentVolume>, sqlx::Error> {
    sqlx::query_as::<_, AgentVolume>(
        "SELECT v.id, v.owner_conversation_id, v.name, v.mount_path
           FROM conversation_volume_mounts m
           JOIN agent_volumes v ON v.id = m.volume_id
          WHERE m.conversation_id = $1 AND v.name = ANY($2)
          ORDER BY v.id
            FOR SHARE OF v",
    )
    .bind(conversation_id)
    .bind(names)
    .fetch_all(&mut *conn)
    .await
}

/// The volumes whose *creating* conversation is in `subtree` (SME-239): the
/// ones a subtree delete takes with it. A conversation that merely mounts one
/// of these volumes isn't in this list — its mount row cascades away and the
/// volume, its rows and its claim stay for whoever created it. That's the
/// difference `delete_subtree`'s claim delete depends on: it deletes these
/// claims and no others.
pub async fn agent_volume_ids_of(pool: &PgPool, subtree: &[i64]) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM agent_volumes WHERE owner_conversation_id = ANY($1) ORDER BY id")
        .bind(subtree)
        .fetch_all(pool)
        .await
}

/// What `delete_volume`'s transaction decided.
#[derive(Debug, PartialEq)]
pub enum DeleteAgentVolume {
    /// The row is gone and its mount rows cascaded with it; `volume` says
    /// what it was, which is what the claim delete and the result text need.
    Deleted(AgentVolume),
    /// This conversation created no volume with that name.
    NotFound,
    /// Another conversation mounting it has an agent `starting` or `active`:
    /// the (conversation, state) pairs, for the refusal's copy.
    HeldBy(Vec<(i64, String)>),
}

/// One `delete_volume`: the row locked `FOR UPDATE`, the refusal check and
/// the delete in the same transaction. That lock is the other half of the
/// pair with `create_child`'s `FOR SHARE` (the design locks the volume rows
/// against a concurrent delete): a start sharing the row first makes the
/// delete see the child, `starting`, and refuse; the delete taking the row
/// first makes the start's lookup find no row and refuse. Either way one of
/// the two refuses, and no pod is left mounted to a volume that isn't there.
/// A conversation that only mounts the volume never gets past the
/// `owner_conversation_id` filter, so its delete is `NotFound` and its own
/// mount row stays.
pub async fn delete_agent_volume_if_unused(
    pool: &PgPool,
    conversation_id: i64,
    name: &str,
) -> Result<DeleteAgentVolume, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT id, owner_conversation_id, mount_path FROM agent_volumes
          WHERE owner_conversation_id = $1 AND name = $2 FOR UPDATE",
    )
    .bind(conversation_id)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, owner, mount_path)) = row else {
        tx.rollback().await?;
        return Ok(DeleteAgentVolume::NotFound);
    };
    let holders = sqlx::query_as::<_, (i64, String)>(
        "SELECT m.conversation_id, a.state
           FROM conversation_volume_mounts m
           JOIN agents a ON a.conversation_id = m.conversation_id
          WHERE m.volume_id = $1 AND m.conversation_id <> $2 AND a.state IN ($3, $4)
          ORDER BY m.conversation_id",
    )
    .bind(id)
    .bind(conversation_id)
    .bind(AGENT_STARTING)
    .bind(AGENT_ACTIVE)
    .fetch_all(&mut *tx)
    .await?;
    if !holders.is_empty() {
        tx.rollback().await?;
        return Ok(DeleteAgentVolume::HeldBy(holders));
    }
    sqlx::query("DELETE FROM agent_volumes WHERE id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(DeleteAgentVolume::Deleted(AgentVolume {
        id,
        owner_conversation_id: owner,
        name: name.to_string(),
        mount_path,
    }))
}
