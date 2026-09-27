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

/// The trust card's answer about request `request_id`, with the hash of
/// the file the card showed. The model is told, and woken to carry on.
#[post("/api/conversations/{id}/instruction-requests/{request_id}")]
pub async fn decide_repo_trust(id: i64, request_id: i64, shown_hash: String, trusted: bool) -> ServerFnResult<()> {
    let pool = db::get();
    let notices = git::decide_trust(pool, id, request_id, &shown_hash, trusted)
        .await
        .map_err(ServerFnError::new)?;
    // In the background: each waits for its conversation's turn to end.
    for (conversation_id, notice) in notices {
        tokio::spawn(async move {
            crate::api::chat::deliver_notice(pool, conversation_id, notice).await;
        });
    }
    Ok(())
}

/// The conversation's repos, for the sandbox panel.
#[get("/api/conversations/{id}/repos")]
pub async fn list_conversation_repos(id: i64) -> ServerFnResult<Vec<RepoSummary>> {
    git::list_repos(db::get(), id).await.map_err(ServerFnError::new)
}

/// "Work on a repo": records the repo and returns at once; the sandbox
/// start and the clone run in the background, so a closed tab can't cut
/// them off. The panel follows them through `ReposUpdate`.
#[post("/api/conversations/{id}/repos")]
pub async fn attach_repo(id: i64, url: String, branch: String) -> ServerFnResult<RepoSummary> {
    let pool = db::get();
    let branch = Some(branch.trim()).filter(|b| !b.is_empty());
    let (shown, pending) = git::start_attach(pool, id, url.trim(), branch)
        .await
        .map_err(ServerFnError::new)?;
    tokio::spawn(async move {
        if let Err(e) = git::finish_attach(pool, id, pending).await {
            tracing::warn!(conversation_id = id, error = %e, "\"Work on a repo\" failed");
        }
    });
    Ok(shown)
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
