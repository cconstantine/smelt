//! The `/secrets` settings page's server functions (SME-242): the user's
//! named secrets, which a persona lists by name in `smelt.credentials`.
//! Write-only the way the ticket settled it (Answer 2): a value goes in
//! and never comes out — what the browser sees is D7's `shown`, computed
//! here at read and never stored — and every successful write reaches
//! every live pod through the same install the keys use.
//!
//! The two validators sit out of `cfg(server)` on purpose: they are the
//! words the page's form says before it submits, so they compile for
//! the browser too. Each restates what the `named_secrets` CHECK
//! enforces (and `credentials::is_credential_name` for a persona's
//! list) — one rule, three tellings, the database the backstop.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::{credentials, db};
#[cfg(feature = "server")]
use sqlx::PgPool;

/// One row of the page: name, dates, and D7's capped display. There is
/// no field a value could sit in, and no endpoint reveals one — the
/// type is the promise (D7, Answer 2).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SecretSummary {
    pub id: i64,
    pub name: String,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    /// The value's safe prefix: at most its first four characters with
    /// at least eight bytes always hidden, plus its byte length.
    pub shown: String,
}

/// The rule `named_secrets.name`'s CHECK spells, shared so the form
/// says the server's words before submitting (the name becomes a file
/// name in a pod). The built-in's own name is refused with its own
/// message: `git-ssh` is a persona's ask, not something you store.
pub fn validate_secret_name(name: &str) -> Result<(), String> {
    if name == "git-ssh" {
        return Err("A secret can't be named git-ssh — that name is smelt's built-in git key.".to_string());
    }
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err("A secret name is 1 to 64 letters, digits, - or _.".to_string());
    }
    Ok(())
}

/// The value's own rule (the column's CHECK, and one the page checks
/// before submitting): never empty, and bounded — 64 KiB is more than
/// any credential smelt hands over, and it keeps a paste of a whole
/// key-file-with-comments out of a pod file.
pub fn validate_secret_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("A secret value can't be empty.".to_string());
    }
    if value.len() > 65_536 {
        return Err("A secret value is at most 65,536 bytes.".to_string());
    }
    Ok(())
}

/// D7's display, computed at read and never stored: with `n` the byte
/// length and `k = min(4, n − 8)` floored at 0, the first `k` **bytes**
/// (trimmed back to a character boundary, never a partial character)
/// and `… (n bytes)`, or just `(n bytes)` when nothing may show. Bytes
/// are the unit on purpose: a `k` counted in bytes but taken in
/// characters could show four bytes per byte counted and leave the
/// eight never-shown ones on display for a non-ASCII lead. Never more
/// than four characters either, and at least eight bytes of every
/// value are never in it — the length is exact on purpose (it's what
/// makes two rows tell apart; Trade-off 9 bounds what a pod could take
/// from it).
#[cfg(feature = "server")]
pub fn shown(value: &str) -> String {
    let n = value.len();
    let k = n.saturating_sub(8).min(4);
    let mut end = k;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let head = &value[..end];
    if head.is_empty() {
        format!("({n} bytes)")
    } else {
        format!("{head}… ({n} bytes)")
    }
}

#[cfg(feature = "server")]
impl From<db::NamedSecret> for SecretSummary {
    fn from(secret: db::NamedSecret) -> Self {
        Self {
            id: secret.id,
            name: secret.name,
            created_at: secret.created_at,
            updated_at: secret.updated_at,
            shown: shown(&secret.value),
        }
    }
}

/// Every stored secret, oldest first — the page's list, values left in
/// the database by construction.
#[get("/api/secrets")]
pub async fn list_secrets() -> ServerFnResult<Vec<SecretSummary>> {
    list_secrets_of(db::get()).await.map_err(ServerFnError::new)
}

/// Adds a secret (no `id`) or changes one row's value (`id`, and the
/// name never changes — renaming is delete and add, Trade-off 7). Every
/// successful write runs the live-pod install, the way a key's does: a
/// persona naming this name gets the file (or loses it) while it runs.
#[post("/api/secrets/save")]
pub async fn save_secret(id: Option<i64>, name: String, value: String) -> ServerFnResult<SecretSummary> {
    save_secret_of(db::get(), id, &name, &value).await.map_err(ServerFnError::new)
}

/// Removes a secret; its file goes from every live pod with it.
#[post("/api/secrets/delete")]
pub async fn delete_secret(id: i64) -> ServerFnResult<()> {
    delete_secret_of(db::get(), id).await.map_err(ServerFnError::new)
}

#[cfg(feature = "server")]
pub(crate) async fn list_secrets_of(pool: &PgPool) -> Result<Vec<SecretSummary>, String> {
    let secrets = db::list_named_secrets(pool).await.map_err(|e| e.to_string())?;
    Ok(secrets.into_iter().map(SecretSummary::from).collect())
}

#[cfg(feature = "server")]
pub(crate) async fn save_secret_of(pool: &PgPool, id: Option<i64>, name: &str, value: &str) -> Result<SecretSummary, String> {
    validate_secret_value(value)?;
    let stored = match id {
        Some(id) => {
            // The name stays what the row has; only the value changes.
            if !db::set_named_secret_value(pool, id, value).await.map_err(|e| e.to_string())? {
                return Err("No secret with that id is stored.".to_string());
            }
            db::get_named_secret(pool, id).await.map_err(|e| e.to_string())?.ok_or("the secret vanished meanwhile")?
        }
        None => {
            validate_secret_name(name)?;
            db::create_named_secret(pool, name, value)
                .await
                .map_err(|e| match e.as_database_error() {
                    // Another submit won the name meanwhile — the page is
                    // the only writer, and the name is immutable, so the
                    // way to reuse a name is to delete it first.
                    Some(db_err) if db_err.is_unique_violation() => {
                        format!("A secret named `{name}` is already stored. Delete it first to store another under that name.")
                    }
                    _ => e.to_string(),
                })?
        }
    };
    credentials::install_into_live_pods(pool).await;
    Ok(SecretSummary::from(stored))
}

#[cfg(feature = "server")]
pub(crate) async fn delete_secret_of(pool: &PgPool, id: i64) -> Result<(), String> {
    db::delete_named_secret(pool, id).await.map_err(|e| e.to_string())?;
    credentials::install_into_live_pods(pool).await;
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests;
