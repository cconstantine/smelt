//! Git support (SME-32): SSH keys and a commit identity installed into
//! every sandbox pod, so the model can clone and push.
//!
//! The summaries at the top cross the client/server boundary (the `/git`
//! settings page), so they're ungated. Key handling and what a pod gets
//! live in the `server`-only module below, re-exported.

use serde::{Deserialize, Serialize};

/// The plain-text rule (SME-276): which characters can break, reorder or
/// hide text, and the escaping that shows them. Moved here verbatim from
/// `git/server/instructions.rs`. Still `server`-gated for now: it becomes
/// client-visible in the next commit, together with the frontend's first
/// call — ungated now, its functions are dead code in the `web` build,
/// which is warning-free.
#[cfg(feature = "server")]
mod plain_text;

/// A stored SSH key as the settings page and the model see it: never the
/// private half.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SshKeySummary {
    pub id: i64,
    pub name: String,
    /// OpenSSH one-line format, ready to paste into GitHub.
    pub public_key: String,
    /// `SHA256:…`, as `ssh-keygen -l` and GitHub show it.
    pub fingerprint: String,
}

/// Where a conversation's repo is: being cloned, checked out, or failed.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoStatus {
    Cloning,
    Ready,
    Failed,
}

/// A repo a conversation works on, as the sandbox panel and the model see it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RepoSummary {
    pub id: i64,
    pub url: String,
    /// Where it's checked out in the pod: `/workspace/<dir>`.
    pub path: String,
    /// The branch asked for; `None` means the remote's default.
    pub requested_branch: Option<String>,
    /// What the last clone checked out.
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub status: RepoStatus,
    /// Why the last clone failed.
    pub error: Option<String>,
    /// The checkout's `AGENTS.md` files, relative to it, top-level first:
    /// what the model can load with `load_instructions`.
    pub agents_files: Vec<String>,
    /// Those of them in the model's context.
    pub loaded_instructions: Vec<String>,
    /// Loads waiting on the user's trust decision, for the trust card.
    pub trust_requests: Vec<TrustRequest>,
}

/// The model asked to load an `AGENTS.md` from a repo the user hasn't
/// decided about: exactly this file is what Trust loads.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrustRequest {
    pub id: i64,
    /// The file, e.g. `/workspace/smelt/AGENTS.md`.
    pub path: String,
    pub content: String,
    /// sha256 of `content`: the card sends it back with the decision, so
    /// Trust loads only the version the user read.
    pub hash: String,
}

/// An `AGENTS.md` loaded into the model's context: what the system prompt
/// carries on every turn, and what the context detail view shows.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProjectInstructions {
    pub repo_url: String,
    /// The file, e.g. `/workspace/smelt/web/AGENTS.md`.
    pub path: String,
    /// The commit checked out when it was loaded.
    pub commit: Option<String>,
    /// At most `INSTRUCTIONS_MAX_BYTES` of it.
    pub content: String,
    /// The whole file's size.
    pub file_bytes: u64,
    /// Only the first 32 KiB of the file were loaded. Set from the file's
    /// size, not `content`'s: invalid bytes decode to longer text.
    pub truncated: bool,
}

/// A remembered trust decision, for the settings page.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RepoTrustSummary {
    /// `git::remote_key`, e.g. `github.com/owner/repo`.
    pub remote: String,
    pub trusted: bool,
}

/// The name and email commits are made with.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

/// A remote as the user wrote it, from its trust key: a local repo's
/// `file/tmp/origin` reads as `file:///tmp/origin` (SME-51 B12). Hosted
/// remotes' keys (`github.com/owner/repo`) already read well.
pub fn remote_label(key: &str) -> String {
    match key.strip_prefix("file/") {
        Some(path) => format!("file:///{path}"),
        None => key.to_string(),
    }
}

#[cfg(test)]
mod label_tests {
    use super::remote_label;

    #[test]
    fn test_a_local_remote_reads_as_its_url() {
        assert_eq!(remote_label("file/workspace/bbsrc"), "file:///workspace/bbsrc");
        assert_eq!(remote_label("github.com/agentsmd/agents.md"), "github.com/agentsmd/agents.md");
    }
}

#[cfg(feature = "server")]
mod server;

#[cfg(feature = "server")]
pub use server::*;
