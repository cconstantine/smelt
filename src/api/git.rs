//! The `/git` settings page's server functions (SME-32): SSH keys and the
//! commit identity. Each is a thin wrapper over `crate::git`, which the
//! model's tools share.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::git::{GitIdentity, RepoSummary, RepoTrustSummary, SshKeySummary};
#[cfg(feature = "server")]
use crate::{db, git};

/// Everything the settings page shows.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GitSettings {
    pub identity: GitIdentity,
    pub keys: Vec<SshKeySummary>,
    pub trust: Vec<RepoTrustSummary>,
}

#[get("/api/git")]
pub async fn get_git_settings() -> ServerFnResult<GitSettings> {
    let pool = db::get();
    Ok(GitSettings {
        identity: db::get_git_identity(pool).await.map_err(ServerFnError::new)?,
        keys: git::list_keys(pool).await.map_err(ServerFnError::new)?,
        trust: db::list_repo_trust(pool)
            .await
            .map_err(ServerFnError::new)?
            .into_iter()
            .map(|t| RepoTrustSummary {
                remote: t.remote_key,
                trusted: t.trusted,
            })
            .collect(),
    })
}

/// Forgets a trust decision: the user is asked again the next time the
/// remote is cloned. What's loaded stays until reloaded.
#[post("/api/git/trust/forget")]
pub async fn forget_repo_trust(remote: String) -> ServerFnResult<()> {
    db::delete_repo_trust(db::get(), &remote)
        .await
        .map_err(ServerFnError::new)
}

/// The trust card's answer. The model is told, and woken to carry on.
#[post("/api/conversations/{id}/repos/{repo_id}/trust")]
pub async fn decide_repo_trust(id: i64, repo_id: i64, trusted: bool) -> ServerFnResult<()> {
    let pool = db::get();
    git::decide_trust(pool, id, repo_id, trusted)
        .await
        .map_err(ServerFnError::new)?;
    tokio::spawn(async move {
        let _ = crate::api::chat::wake_conversation(pool, id).await;
    });
    Ok(())
}

/// Reload: loads what the checkout's AGENTS.md says now.
#[post("/api/conversations/{id}/repos/{repo_id}/reload")]
pub async fn reload_repo_instructions(id: i64, repo_id: i64) -> ServerFnResult<()> {
    git::reload_instructions(db::get(), id, repo_id)
        .await
        .map_err(ServerFnError::new)
}

/// The conversation's repos, for the sandbox panel.
#[get("/api/conversations/{id}/repos")]
pub async fn list_conversation_repos(id: i64) -> ServerFnResult<Vec<RepoSummary>> {
    git::list_repos(db::get(), id).await.map_err(ServerFnError::new)
}

/// "Work on a repo": starts the sandbox if needed and clones `url`.
#[post("/api/conversations/{id}/repos")]
pub async fn attach_repo(id: i64, url: String, branch: String) -> ServerFnResult<RepoSummary> {
    let branch = Some(branch.trim()).filter(|b| !b.is_empty());
    git::attach_repo(db::get(), id, url.trim(), branch)
        .await
        .map_err(ServerFnError::new)
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
