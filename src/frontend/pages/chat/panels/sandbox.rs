//! The sandbox panel: pods, terminals, repos.

use super::super::*;

/// What the sandbox panel says about a repo's `AGENTS.md` files, if it
/// has any: which the model has loaded.
pub(in super::super) fn instructions_label(repo: &RepoSummary) -> Option<String> {
    if !repo.loaded_instructions.is_empty() {
        Some(format!("Loaded {}", repo.loaded_instructions.join(", ")))
    } else if !repo.agents_files.is_empty() {
        Some("AGENTS.md not loaded".to_string())
    } else {
        None
    }
}

pub(in super::super) fn repo_status_class(status: RepoStatus) -> &'static str {
    match status {
        RepoStatus::Cloning => "cloning",
        RepoStatus::Ready => "ready",
        RepoStatus::Failed => "failed",
    }
}

/// A repo's state under its path in the sandbox panel.
pub(in super::super) fn repo_detail(repo: &RepoSummary) -> String {
    match repo.status {
        RepoStatus::Cloning => match &repo.requested_branch {
            Some(branch) => format!("Cloning {branch}\u{2026}"),
            None => "Cloning\u{2026}".to_string(),
        },
        RepoStatus::Failed => "Clone failed".to_string(),
        RepoStatus::Ready => {
            let branch = repo.branch.clone().unwrap_or_default();
            let commit: String = repo.commit.clone().unwrap_or_default().chars().take(7).collect();
            format!("{branch} \u{b7} {commit}")
        }
    }
}
