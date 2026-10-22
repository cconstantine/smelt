//! A checkout's persona files (SME-235): found at clone like its
//! AGENTS.md files, and named in the system prompt. The format itself is
//! `crate::personas`'s alone.

use super::*;
use std::collections::BTreeMap;

/// Where a repo's persona files are, relative to its checkout.
pub const PERSONAS_DIR: &str = ".agents/personas/";

/// How many persona files of one checkout are recorded, as for AGENTS.md.
const MAX_PERSONA_FILES: usize = 50;

/// The persona files among `git ls-files -z` output: the `.md` files
/// directly in `.agents/personas/`, sorted, at most 50. Git's pathspec
/// `*` matches `/` too, so a file in a subdirectory comes through the
/// listing and is dropped here.
pub(super) fn persona_paths(listed: &str) -> Vec<String> {
    let mut files: Vec<String> = listed
        .split('\0')
        .filter(|path| {
            path.strip_prefix(PERSONAS_DIR)
                .is_some_and(|name| !name.contains('/') && name.ends_with(".md") && name != ".md")
        })
        .map(str::to_string)
        .collect();
    files.sort();
    files.truncate(MAX_PERSONA_FILES);
    files
}

/// The checkout's tracked persona files, relative to it.
pub async fn list_persona_files(shell: &sandbox::PodShell, dir: &str) -> Result<Vec<String>, String> {
    let path = format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR);
    let pathspec = format!("{PERSONAS_DIR}*.md");
    let listed = shell
        .run(&["git", "-C", &path, "ls-files", "-z", "--", &pathspec], None)
        .await
        .map_err(|e| e.to_string())?;
    if listed.exit_code != 0 {
        return Err(format!("couldn't list {path}'s persona files: {}", listed.stderr.trim()));
    }
    Ok(persona_paths(&listed.stdout))
}

/// Each of the conversation's repos' recorded persona files, by repo id,
/// for the system prompt: names only, whether or not the repo is trusted.
pub async fn persona_files_by_repo(pool: &PgPool, conversation_id: i64) -> Result<BTreeMap<i64, Vec<String>>, String> {
    let repos = db::list_conversation_repos(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(repos.into_iter().map(|r| (r.id, r.persona_files)).collect())
}
