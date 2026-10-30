//! The user's named secrets (SME-242): one row per credential a persona
//! may name in `smelt.credentials`. The page is the only writer and is
//! write-only (D7); every other reader is an install, a report or a
//! Rule S resolution, and none of them binds a value to a conversation
//! — the conversation binding is Rule G/Rule S's name set, decided
//! before any value is read (SME-242, state model).
//!
//! The `name` is immutable (renaming is delete and add, Trade-off 7),
//! and no read here ever truncates or derives from a value: the page's
//! safe prefix (`shown`) is D7's, computed in `api::secrets` at read,
//! never stored.

use super::*;

/// One named secret, whole. `Debug` is hand-written and hides `value`:
/// nothing that Debugs this row may print what it holds.
#[derive(Clone)]
pub struct NamedSecret {
    pub id: i64,
    pub name: String,
    pub value: String,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

impl std::fmt::Debug for NamedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedSecret")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("value", &"[hidden]")
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// The column list every read shares, and the row it decodes to.
const ROW_SQL: &str = "id, name, value, created_at, updated_at";

fn row(
    id: i64,
    name: String,
    value: String,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
) -> NamedSecret {
    NamedSecret { id, name, value, created_at, updated_at }
}

/// Every stored secret, oldest first — the read every install and every
/// Rule S resolution makes, and the page's list. (The id names no
/// cluster object, so `db/git.rs`'s `CLEAR_ID_TABLES` stays as it is.)
pub async fn list_named_secrets(pool: &PgPool) -> Result<Vec<NamedSecret>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, String, String, chrono::NaiveDateTime, chrono::NaiveDateTime)>(&format!(
        "SELECT {ROW_SQL} FROM named_secrets ORDER BY created_at ASC, name ASC",
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id, name, value, created_at, updated_at)| row(id, name, value, created_at, updated_at)).collect())
}

/// One secret by id: what a value-change checks against before writing,
/// since the name never changes and the row must be there to change.
pub async fn get_named_secret(pool: &PgPool, id: i64) -> Result<Option<NamedSecret>, sqlx::Error> {
    let found = sqlx::query_as::<_, (i64, String, String, chrono::NaiveDateTime, chrono::NaiveDateTime)>(&format!(
        "SELECT {ROW_SQL} FROM named_secrets WHERE id = $1",
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(found.map(|(id, name, value, created_at, updated_at)| row(id, name, value, created_at, updated_at)))
}

/// Stores a new secret. A name already stored is the unique violation
/// for the caller to turn into its refusal — the page is the only
/// writer, so one arriving with a taken name is a double submit.
pub async fn create_named_secret(pool: &PgPool, name: &str, value: &str) -> Result<NamedSecret, sqlx::Error> {
    let created = sqlx::query_as::<_, (i64, String, String, chrono::NaiveDateTime, chrono::NaiveDateTime)>(&format!(
        "INSERT INTO named_secrets (name, value) VALUES ($1, $2) RETURNING {ROW_SQL}",
    ))
    .bind(name)
    .bind(value)
    .fetch_one(pool)
    .await?;
    Ok(row(created.0, created.1, created.2, created.3, created.4))
}

/// Replaces one secret's value, leaving its name (its `updated_at`
/// rides). False when no row has `id` — nothing to change.
pub async fn set_named_secret_value(pool: &PgPool, id: i64, value: &str) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query_scalar::<_, i64>("UPDATE named_secrets SET value = $2, updated_at = now() WHERE id = $1 RETURNING id")
        .bind(id)
        .bind(value)
        .fetch_optional(pool)
        .await?;
    Ok(changed.is_some())
}

/// Removes a secret. False when no row has `id` — already gone, which
/// the page's Delete racing another tab is, not an error to raise.
pub async fn delete_named_secret(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query_scalar::<_, i64>("DELETE FROM named_secrets WHERE id = $1 RETURNING id")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(deleted.is_some())
}
