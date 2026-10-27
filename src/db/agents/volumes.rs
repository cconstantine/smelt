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
