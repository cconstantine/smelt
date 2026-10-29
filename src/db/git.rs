use super::*;

// --- Git: SSH keys and the commit identity (SME-32) ---

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct SshKey {
    pub id: i64,
    pub name: String,
    pub public_key: String,
    /// Plain text for now: see SME-48.
    pub private_key: String,
    pub created_at: NaiveDateTime,
}

pub async fn create_ssh_key(
    pool: &PgPool,
    name: &str,
    public_key: &str,
    private_key: &str,
) -> Result<SshKey, sqlx::Error> {
    sqlx::query_as::<_, SshKey>(
        "INSERT INTO ssh_keys (name, public_key, private_key) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(name)
    .bind(public_key)
    .bind(private_key)
    .fetch_one(pool)
    .await
}

pub async fn list_ssh_keys(pool: &PgPool) -> Result<Vec<SshKey>, sqlx::Error> {
    sqlx::query_as::<_, SshKey>("SELECT * FROM ssh_keys ORDER BY name ASC")
        .fetch_all(pool)
        .await
}

pub async fn delete_ssh_key(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM ssh_keys WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The commit identity; empty strings when it has never been set.
pub async fn get_git_identity(pool: &PgPool) -> Result<crate::git::GitIdentity, sqlx::Error> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT author_name, author_email FROM git_settings")
            .fetch_optional(pool)
            .await?;
    Ok(row
        .map(|(name, email)| crate::git::GitIdentity { name, email })
        .unwrap_or_default())
}

pub async fn set_git_identity(
    pool: &PgPool,
    identity: &crate::git::GitIdentity,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO git_settings (author_name, author_email) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE
         SET author_name = $1, author_email = $2, updated_at = now()",
    )
    .bind(&identity.name)
    .bind(&identity.email)
    .execute(pool)
    .await?;
    Ok(())
}

// --- This database's identity in the cluster (SME-115) ---

/// Which database a cluster object belongs to. Every smelt server shares
/// the sandbox namespace, and its objects are named after this database's
/// ids, so each one is labelled `smelt/instance=<id>` and a server only
/// deletes, reuses, mounts or watches objects carrying its own id.
#[derive(Clone, Debug, PartialEq)]
pub struct SmeltInstance {
    /// A UUID, made when the database was migrated.
    pub id: String,
    /// Whether this database owns the objects made before SME-115, which
    /// carry no instance label: true only for a database that already had
    /// conversations or volumes when it migrated (the dev and production
    /// databases), fixed then. Only such a database adopts them.
    pub owns_unlabelled: bool,
}

/// The migration that made `smelt_instance` (its version).
#[cfg(test)]
pub(super) const SMELT_INSTANCE_MIGRATION: i64 = 20261007200000;

/// This database's `smelt_instance` row.
pub async fn smelt_instance(pool: &PgPool) -> Result<SmeltInstance, sqlx::Error> {
    let (id, owns_unlabelled): (String, bool) =
        sqlx::query_as("SELECT instance_id::text, owns_unlabelled FROM smelt_instance")
            .fetch_one(pool)
            .await?;
    Ok(SmeltInstance { id, owns_unlabelled })
}

/// Starts a fresh database's conversation, sandbox pod and sandbox volume
/// ids at a high base of their own (SME-115), and returns it; `None`, and
/// nothing changed, for a database any of them was ever used in (the dev
/// and production databases). Cluster objects are named after these ids,
/// and every smelt server shares the namespace: a scratch database
/// counting from 1 would name its claims and pods like the dev server's.
pub async fn start_ids_clear_if_fresh(pool: &PgPool) -> Result<Option<i64>, sqlx::Error> {
    // Each sequence's last value: null until its first one.
    let sequences: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT t.name, s.last_value
         FROM unnest($1::text[]) AS t(name)
         JOIN pg_sequences s ON format('%I.%I', s.schemaname, s.sequencename) = pg_get_serial_sequence(t.name, 'id')",
    )
    .bind(&CLEAR_ID_TABLES[..])
    .fetch_all(pool)
    .await?;
    // Any id ever handed out below the base: a used database, left alone.
    // One already at or above it was moved by an earlier start; a move
    // that failed partway (`setval` isn't transactional) is finished now
    // (SME-115 review 2).
    if sequences.len() != CLEAR_ID_TABLES.len()
        || sequences.iter().any(|(_, last)| last.is_some_and(|last| last < CLEAR_IDS_BASE))
    {
        return Ok(None);
    }
    let unused: Vec<&str> = sequences.iter().filter(|(_, last)| last.is_none()).map(|(name, _)| name.as_str()).collect();
    if unused.is_empty() {
        return Ok(None);
    }
    let base = clear_ids_base();
    for table in unused {
        sqlx::query("SELECT setval(pg_get_serial_sequence($1, 'id'), $2)")
            .bind(table)
            .bind(base)
            .execute(pool)
            .await?;
    }
    Ok(Some(base))
}

/// The tables whose ids name cluster objects: claims and pod labels after
/// the conversation, `sandbox-{id}` pods, volume claims — global ones and
/// the model's (SME-239), whose `agent-volume-<id>` claims two parallel
/// test databases otherwise collide over.
const CLEAR_ID_TABLES: [&str; 4] = ["conversations", "sandbox_pods", "sandbox_volumes", "agent_volumes"];

/// Far above the low ids a database counts from (the dev database is in
/// the thousands, every fresh `#[sqlx::test]` one starts at 1). Nothing
/// a test run hands out sits below it: since SME-255 every harness move
/// and every fixture id comes from `crate::id_alloc`'s bands at 2e9 and
/// above.
const CLEAR_IDS_BASE: i64 = 2_000_000_000;

/// A fresh base for this database's id sequences: the next value of the
/// `[2e9, 3e9)` band `crate::id_alloc` allocates (SME-255). No two
/// callers share a base, and consecutive ones sit `id_alloc::ID_STRIDE`
/// apart — the fold it replaces (`CLEAR_IDS_BASE + nanos % 1e9`) handed
/// two near-simultaneous callers the same base whenever their clock reads
/// shared a tick, and their pods met by name in the shared test
/// namespace.
pub(super) fn clear_ids_base() -> i64 {
    crate::id_alloc::GLOBAL_IDS.next_base()
}

/// Moves **every** sequence in the test database to `base`: the next id
/// in any of them is above it. Not the `CLEAR_ID_TABLES` list — the
/// production path keeps that (their ids are the ones that name cluster
/// objects there, and the used-database check reads exactly those) —
/// the test path moves everything structurally, so the next table whose
/// ids name a cluster object is clear of other runs without anyone
/// extending a list (SME-255; two test databases naming `agent-volume-1`
/// was this class, met).
#[cfg(test)]
pub(super) async fn move_id_sequences(pool: &PgPool, base: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT setval(format('%I.%I', schemaname, sequencename)::regclass, $1)
         FROM pg_sequences
         WHERE schemaname = 'public'",
    )
    .bind(base)
    .execute(pool)
    .await?;
    Ok(())
}
