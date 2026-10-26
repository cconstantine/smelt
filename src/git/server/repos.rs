use super::*;

/// The identity of a remote repository, whatever URL form named it:
/// `git@github.com:o/r.git`, `ssh://git@github.com/o/r`,
/// `https://github.com/o/r.git` and `https://github.com/O/R/` are all
/// `github.com/o/r`. What trust decisions are remembered by. The host
/// is lowercased, and the path too on hosts that ignore its case. `None`
/// for anything that isn't a clonable URL.
pub fn remote_key(url: &str) -> Option<String> {
    let (host, path) = parse_remote(url)?;
    let host = host.to_lowercase();
    // Only these hosts treat owner and repo names without regard to
    // case; elsewhere `Team/Repo` and `team/repo` can be two repos, and
    // must not share a trust decision.
    let path = if CASE_INSENSITIVE_HOSTS.contains(&host.as_str()) {
        path.to_lowercase()
    } else {
        path
    };
    Some(format!("{host}/{path}"))
}

const CASE_INSENSITIVE_HOSTS: [&str; 3] = ["github.com", "gitlab.com", "bitbucket.org"];

/// The directory a clone of `url` goes in, under `/workspace`: the
/// repo's own name, like `git clone` picks.
pub fn default_checkout_dir(url: &str) -> Option<String> {
    let (_, path) = parse_remote(url)?;
    path.rsplit('/').next().map(str::to_string)
}

/// `(host, path)` of a clonable URL, the path without slashes at
/// either end or a `.git` suffix; `file` as the host for a local path.
/// Anything git would read as an option or a transport helper
/// (`ext::…`) is refused, since the URL ends up on a command line.
pub(super) fn parse_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if url.is_empty() || url.starts_with('-') || url.contains("::") || url.contains(char::is_whitespace) {
        return None;
    }
    let (host, path) = if url.contains("://") {
        let parsed = url::Url::parse(url).ok()?;
        match parsed.scheme() {
            "file" => ("file".to_string(), parsed.path().to_string()),
            "https" | "http" | "ssh" | "git" => {
                (parsed.host_str()?.to_string(), parsed.path().to_string())
            }
            _ => return None,
        }
    } else {
        // scp-like: [user@]host:path, where a `/` before the `:` would
        // make it a local path instead.
        let (before, path) = url.split_once(':')?;
        if before.contains('/') {
            return None;
        }
        let host = before.rsplit_once('@').map_or(before, |(_, host)| host);
        (host.to_string(), path.to_string())
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some((host, path.to_string()))
}

fn repo_summary(
    repo: db::ConversationRepo,
    loaded: Vec<String>,
    requests: Vec<db::InstructionRequest>,
) -> RepoSummary {
    let path = format!("{}/{}", crate::sandbox::WORKSPACE_DIR, repo.dir);
    RepoSummary {
        trust_requests: requests
            .into_iter()
            .map(|r| TrustRequest {
                id: r.id,
                // A repo card quotes the checkout dir itself, not a
                // file under it (SME-279).
                path: if r.path.is_empty() { path.clone() } else { format!("{path}/{}", r.path) },
                content: r.content,
                hash: r.hash,
                is_repo: r.path.is_empty(),
            })
            .collect(),
        loaded_instructions: loaded,
        agents_files: repo.agents_files,
        id: repo.id,
        url: repo.url,
        path,
        requested_branch: repo.branch,
        branch: repo.checked_out_branch,
        commit: repo.commit_sha,
        status: match repo.status.as_str() {
            "ready" => RepoStatus::Ready,
            "failed" => RepoStatus::Failed,
            _ => RepoStatus::Cloning,
        },
        error: repo.error,
    }
}

/// A conversation's repos.
pub async fn list_repos(pool: &PgPool, conversation_id: i64) -> Result<Vec<RepoSummary>, String> {
    let repos = db::list_conversation_repos(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let loaded = db::list_loaded_instructions(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let requests = db::list_instruction_requests(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(repos
        .into_iter()
        .map(|repo| {
            let mine = |id: i64| id == repo.id;
            let loaded = loaded.iter().filter(|l| mine(l.repo_id)).map(|l| l.path.clone()).collect();
            let requests = requests.iter().filter(|r| mine(r.repo_id)).cloned().collect();
            repo_summary(repo, loaded, requests)
        })
        .collect())
}

pub(super) async fn summarise(pool: &PgPool, repo: db::ConversationRepo) -> Result<RepoSummary, String> {
    let id = repo.id;
    list_repos(pool, repo.conversation_id)
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| "the repo vanished".to_string())
}

/// Waits, up to `timeout`, while any of the conversation's repos is
/// still cloning, so a turn starts knowing the repo is there. Returns
/// whether none is cloning any more.
pub async fn wait_for_clones(pool: &PgPool, conversation_id: i64, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let cloning = match db::list_conversation_repos(pool, conversation_id).await {
            Ok(repos) => repos.iter().any(|r| r.status == "cloning"),
            Err(e) => {
                tracing::warn!(conversation_id, error = %e, "couldn't check for clones in progress");
                return true;
            }
        };
        if !cloning {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub(super) async fn get_repo(pool: &PgPool, repo_id: i64) -> Result<db::ConversationRepo, String> {
    db::get_conversation_repo(pool, repo_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "No such repo.".to_string())
}
