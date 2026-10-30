//! The user's named secrets (SME-242): one row per credential a persona
//! may name in `smelt.credentials`. The page is the only writer and is
//! write-only (D7); every other reader is an install, a report or a
//! Rule S resolution, and none of them binds a value to a conversation
//! — the conversation binding is Rule G/Rule S's name set, decided
//! before any value is read (docs/plans/sme-242.md, state model).
//!
//! Creation, edit and delete arrive with the page's API (`api::secrets`):
//! until a caller outside its tests exists, the compiler would rightly
//! call these dead.

use super::*;

/// One named secret. `Debug` is hand-written and hides `value`: nothing
/// that Debugs this row may print what it holds.
pub struct NamedSecret {
    pub name: String,
    pub value: String,
}

impl std::fmt::Debug for NamedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedSecret").field("name", &self.name).field("value", &"[hidden]").finish()
    }
}

/// Every stored secret, oldest first — the read every install and every
/// Rule S resolution makes. Today only `name` and `value` leave here;
/// the page's dates come with the page (and the id names no cluster
/// object, so `db/git.rs`'s `CLEAR_ID_TABLES` stays as it is).
pub async fn list_named_secrets(pool: &PgPool) -> Result<Vec<NamedSecret>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT name, value FROM named_secrets ORDER BY created_at ASC, name ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name, value)| NamedSecret { name, value }).collect())
}
