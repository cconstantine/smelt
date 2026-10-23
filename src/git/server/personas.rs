//! A checkout's persona files (SME-235): found at clone like its
//! AGENTS.md files, and named in the system prompt. The format itself is
//! `crate::personas`'s alone.

use super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Where a repo's persona files are, relative to its checkout.
pub const PERSONAS_DIR: &str = ".agents/personas/";

/// How many persona files of one checkout are recorded, as for AGENTS.md.
const MAX_PERSONA_FILES: usize = 50;

/// The persona files a checkout's listing found, and how many other files
/// directly in `.agents/personas/` weren't taken (a name no persona can
/// have, or past the first 50), so a listing can say so.
#[derive(Debug, PartialEq)]
pub struct PersonaPaths {
    pub files: Vec<String>,
    pub unread: usize,
}

/// The persona files among `git ls-files -z` output: the files directly
/// in `.agents/personas/` named as a persona's can be (`<name>.md`),
/// sorted, at most 50. Git's pathspec `*` matches `/` too, so a file in a
/// subdirectory comes through the listing and is dropped here.
pub(super) fn persona_paths(listed: &str) -> PersonaPaths {
    let direct: Vec<&str> = listed
        .split('\0')
        .filter_map(|path| path.strip_prefix(PERSONAS_DIR).filter(|name| !name.is_empty() && !name.contains('/')).map(|_| path))
        .collect();
    let mut files: Vec<String> = direct
        .iter()
        .filter(|path| path.strip_prefix(PERSONAS_DIR).is_some_and(crate::personas::is_persona_file_name))
        .map(|path| path.to_string())
        .collect();
    files.sort();
    files.truncate(MAX_PERSONA_FILES);
    PersonaPaths { unread: direct.len() - files.len(), files }
}

/// The checkout's tracked persona files, relative to it.
pub async fn list_persona_files(shell: &sandbox::PodShell, dir: &str) -> Result<PersonaPaths, String> {
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

/// The persona files recorded for repo `repo_id`, for the clone's result;
/// none when they can't be read.
pub async fn repo_persona_files(pool: &PgPool, repo_id: i64) -> Vec<String> {
    match db::get_conversation_repo(pool, repo_id).await {
        Ok(repo) => repo.map(|r| r.persona_files).unwrap_or_default(),
        Err(e) => {
            tracing::warn!(repo_id, error = %e, "couldn't read a repo's persona files");
            Vec::new()
        }
    }
}

/// One persona file `list_personas` read, and what parsing it gave.
pub(crate) struct PersonaFile {
    /// Relative to the checkout.
    pub path: String,
    pub parsed: Result<crate::personas::Persona, String>,
}

/// A checkout's persona files as read, and how many other files in the
/// directory weren't read (`PersonaPaths::unread`).
pub(crate) struct CheckoutPersonas {
    pub files: Vec<PersonaFile>,
    pub unread: usize,
}

/// What `list_personas` read from one repo's checkout, before the trust
/// gate: `None` unless the repo is ready; an error when it couldn't be
/// listed.
pub(crate) struct RepoPersonas {
    pub repo: db::ConversationRepo,
    pub read: Option<Result<CheckoutPersonas, String>>,
}

/// What the model is told when the conversation has no repos.
const NO_REPOS: &str = "This conversation has no repos, so it has no personas. Clone one with clone_repo.";

/// The model's `list_personas` tool: each of the conversation's repos'
/// personas, read from its checkout now, listed only for a repo whose
/// remote the user trusts. Re-records each ready repo's persona files.
pub async fn list_personas(pool: &PgPool, conversation_id: i64) -> Result<String, String> {
    let repos = db::list_conversation_repos(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    if repos.is_empty() {
        return Ok(NO_REPOS.to_string());
    }
    let shell = if repos.iter().any(|r| r.status == "ready") {
        Some(
            sandbox::PodShell::for_conversation(pool, conversation_id)
                .await
                .map_err(no_shell_message)?,
        )
    } else {
        None
    };
    let mut reads = Vec::with_capacity(repos.len());
    for repo in repos {
        let read = match &shell {
            Some(shell) if repo.status == "ready" => Some(read_repo_personas(pool, shell, &repo).await),
            _ => None,
        };
        reads.push(RepoPersonas { repo, read });
    }
    gated_listing(pool, reads).await
}

/// Reads and parses every persona file in `repo`'s checkout, and
/// re-records the list. Nothing here reaches the model before the gate.
async fn read_repo_personas(
    pool: &PgPool,
    shell: &sandbox::PodShell,
    repo: &db::ConversationRepo,
) -> Result<CheckoutPersonas, String> {
    let PersonaPaths { files: paths, unread } = list_persona_files(shell, &repo.dir).await?;
    if let Err(e) = db::set_repo_persona_files(pool, repo.id, repo.attempt, &paths).await {
        tracing::warn!(repo_id = repo.id, error = %e, "couldn't re-record persona files");
    }
    let checkout = format!("{}/{}", crate::sandbox::WORKSPACE_DIR, repo.dir);
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        // Symlink-safe, as for an AGENTS.md: a persona file that is a
        // symlink, or leads outside the checkout, is refused.
        let parsed = match read_instructions_file(shell, &checkout, &format!("{checkout}/{path}")).await {
            Ok(Some(file)) => crate::personas::parse(&path, &file.content, file.file_bytes.max(0) as u64),
            Ok(None) => Err(format!("{path} is no longer there.")),
            Err(e) => Err(e),
        };
        files.push(PersonaFile { path, parsed });
    }
    Ok(CheckoutPersonas { files, unread })
}

/// Whether the user trusts `remote_key`: `None` when they haven't
/// decided. Read under the remote's trust lock, as a load is (SME-181).
async fn remote_trust(pool: &PgPool, remote_key: &str) -> Result<Option<bool>, String> {
    let e = |e: sqlx::Error| e.to_string();
    let mut tx = pool.begin().await.map_err(e)?;
    db::lock_remote_trust(&mut *tx, remote_key).await.map_err(e)?;
    let trusted = db::get_repo_trust(&mut *tx, remote_key).await.map_err(e)?;
    tx.commit().await.map_err(e)?;
    Ok(trusted)
}

/// One persona as the model sees it: named `<dir>:<name>`, at its
/// absolute path; a file that doesn't parse, with its error.
fn persona_entry(dir: &str, file: PersonaFile) -> Value {
    let full = format!("{}/{dir}/{}", crate::sandbox::WORKSPACE_DIR, file.path);
    match file.parsed {
        Ok(persona) => {
            let mut entry = json!({
                "persona": format!("{dir}:{}", persona.name),
                "description": persona.description,
                "file": full,
            });
            if let Some(model) = persona.model {
                entry["model"] = Value::String(model);
            }
            entry
        }
        Err(error) => json!({"file": full, "error": error}),
    }
}

/// What the model is told: each repo's personas, only for a repo whose
/// remote the user trusts, read under the remote's trust lock. Nothing
/// from inside an untrusted repo's files (a description, a model, a parse
/// error quoting the file) is in it.
pub(crate) async fn gated_listing(pool: &PgPool, reads: Vec<RepoPersonas>) -> Result<String, String> {
    if reads.is_empty() {
        return Ok(NO_REPOS.to_string());
    }
    let mut any_persona = false;
    let mut repos = Vec::with_capacity(reads.len());
    for RepoPersonas { repo, read } in reads {
        let path = format!("{}/{}", crate::sandbox::WORKSPACE_DIR, repo.dir);
        let mut entry = json!({"path": path, "url": repo.url});
        let Some(read) = read else {
            entry["note"] = Value::String(match repo.status.as_str() {
                "cloning" => "It's still cloning: call list_personas again once it's ready.".to_string(),
                // SME-275: the error can be git's output, which the remote can
                // write into, so what breaks or reorders a line goes escaped.
                _ => format!(
                    "Its clone failed: {}",
                    repo.error.as_deref().map_or_else(|| "no reason recorded".to_string(), plain_multiline_text)
                ),
            });
            repos.push(entry);
            continue;
        };
        match remote_trust(pool, &repo.remote_key).await? {
            Some(true) => match read {
                Ok(CheckoutPersonas { files, unread }) => {
                    let mut notes = Vec::new();
                    if files.is_empty() {
                        notes.push(format!("It has no persona files ({PERSONAS_DIR}*.md)."));
                    } else {
                        let personas: Vec<Value> = files.into_iter().map(|f| persona_entry(&repo.dir, f)).collect();
                        any_persona |= personas.iter().any(|p| p.get("persona").is_some());
                        entry["personas"] = Value::Array(personas);
                    }
                    if unread > 0 {
                        let files = if unread == 1 { "file there wasn't" } else { "files there weren't" };
                        notes.push(format!(
                            "{unread} other .md {files} read from {PERSONAS_DIR}: a persona file is named <name>.md, \
                             the name 1 to 64 of a-z, 0-9 and - (not starting or ending with -), and at most \
                             {MAX_PERSONA_FILES} are read."
                        ));
                    }
                    if !notes.is_empty() {
                        entry["note"] = Value::String(notes.join(" "));
                    }
                }
                Err(e) => entry["error"] = Value::String(format!("Couldn't list its persona files: {e}")),
            },
            Some(false) => {
                entry["note"] = Value::String(format!(
                    "The user doesn't trust {}, so its personas aren't available.",
                    repo.url
                ));
            }
            None if repo.agents_files.is_empty() => {
                entry["note"] = Value::String(format!(
                    "The user hasn't decided whether to trust {}, so its personas aren't shown. It has \
                     no AGENTS.md to ask with: the user can trust it by opening it with Work on a repo.",
                    repo.url
                ));
            }
            None => {
                entry["note"] = Value::String(format!(
                    "The user hasn't decided whether to trust {}, so its personas aren't shown. Loading \
                     its AGENTS.md with load_instructions asks them; then call list_personas again.",
                    repo.url
                ));
            }
        }
        repos.push(entry);
    }
    let mut told = json!({"repos": repos});
    if any_persona {
        told["note"] = Value::String(
            "A persona is named <repo dir>:<name>. It is used by starting an agent with it, which isn't available yet."
                .to_string(),
        );
    }
    Ok(told.to_string())
}
