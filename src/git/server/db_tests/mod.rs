//! The git server's database tests: shared fixtures here, the tests in
//! topic files by subject (SME-279 split the one file it outgrew).

use super::*;

/// A listing that found no AGENTS.md files.
fn no_agents() -> AgentsPaths {
    AgentsPaths { files: vec![], left_out: 0 }
}

pub(crate) fn file(content: &str) -> db::InstructionsFile {
    db::InstructionsFile {
        content: content.to_string(),
        file_bytes: content.len() as i64,
        hash: format!("hash-{content}"),
        commit: Some("abc123".to_string()),
    }
}

pub(crate) async fn cloned_repo(pool: &PgPool, conversation_id: i64) -> db::ConversationRepo {
    let repo = db::create_conversation_repo(pool, conversation_id, "git@github.com:o/r.git", "github.com/o/r", None, "r")
        .await
        .expect("repo");
    db::set_repo_cloned(pool, repo.id, "main", Some("abc123")).await.expect("cloned");
    db::get_conversation_repo(pool, repo.id).await.expect("get").expect("exists")
}

pub(super) async fn only_repo(pool: &PgPool, conversation_id: i64) -> RepoSummary {
    let mut repos = list_repos(pool, conversation_id).await.expect("list");
    assert_eq!(repos.len(), 1, "{repos:?}");
    repos.remove(0)
}

mod keys;
mod clones;
mod attempts;
mod decisions;
mod work_on_repo;

