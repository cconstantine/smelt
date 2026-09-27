//! The `/git` settings page's server functions (SME-32): SSH keys and the
//! commit identity. Each is a thin wrapper over `crate::git`, which the
//! model's tools share.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::git::{GitIdentity, SshKeySummary};
#[cfg(feature = "server")]
use crate::{db, git};

/// Everything the settings page shows.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GitSettings {
    pub identity: GitIdentity,
    pub keys: Vec<SshKeySummary>,
}

#[get("/api/git")]
pub async fn get_git_settings() -> ServerFnResult<GitSettings> {
    let pool = db::get();
    Ok(GitSettings {
        identity: db::get_git_identity(pool).await.map_err(ServerFnError::new)?,
        keys: git::list_keys(pool).await.map_err(ServerFnError::new)?,
    })
}

#[post("/api/git/identity")]
pub async fn save_git_identity(identity: GitIdentity) -> ServerFnResult<()> {
    git::save_identity(db::get(), &identity)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/git/keys")]
pub async fn generate_ssh_key(name: String) -> ServerFnResult<SshKeySummary> {
    git::create_key(db::get(), name.trim())
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/git/keys/import")]
pub async fn import_ssh_key(name: String, private_key: String) -> ServerFnResult<SshKeySummary> {
    git::import_key_named(db::get(), name.trim(), &private_key)
        .await
        .map_err(ServerFnError::new)
}

#[delete("/api/git/keys/{id}")]
pub async fn delete_ssh_key(id: i64) -> ServerFnResult<()> {
    git::delete_key(db::get(), id)
        .await
        .map_err(ServerFnError::new)
}
