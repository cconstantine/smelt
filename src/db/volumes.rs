use super::*;

// --- Sandbox volumes (generic pod mounts, configured via the
// /sandbox-volumes UI) --- Plain CRUD, no soft delete — configuration a
// person edits, same precedent as mcp_servers below, not a live external
// resource like a sandbox pod itself. See
// SME-17's Phase 4.

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct SandboxVolume {
    pub id: i64,
    pub name: String,
    /// Already-resolved (any leading `~` expanded to the sandbox user's
    /// home directory before this row is ever written) — a plain absolute
    /// path, set once at creation and never changed after.
    pub mount_path: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

pub async fn create_sandbox_volume(
    pool: &PgPool,
    name: &str,
    mount_path: &str,
) -> Result<SandboxVolume, sqlx::Error> {
    sqlx::query_as::<_, SandboxVolume>(
        "INSERT INTO sandbox_volumes (name, mount_path) VALUES ($1, $2) RETURNING *",
    )
    .bind(name)
    .bind(mount_path)
    .fetch_one(pool)
    .await
}

/// Every configured volume — unlike `list_sandbox_pods`, not scoped to a
/// conversation at all: a volume is defined once and every pod smelt
/// creates gets every one of these mounted, per the idea's "not something
/// chosen per conversation."
pub async fn list_sandbox_volumes(pool: &PgPool) -> Result<Vec<SandboxVolume>, sqlx::Error> {
    sqlx::query_as::<_, SandboxVolume>("SELECT * FROM sandbox_volumes ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn get_sandbox_volume(
    pool: &PgPool,
    id: i64,
) -> Result<Option<SandboxVolume>, sqlx::Error> {
    sqlx::query_as::<_, SandboxVolume>("SELECT * FROM sandbox_volumes WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn delete_sandbox_volume(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sandbox_volumes WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
