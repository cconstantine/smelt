use super::*;

/// The command that clones `url` into `path`: under coreutils
/// `timeout`, so the pod itself stops a clone that runs too long (git
/// then removes its partial checkout), rather than smelt only dropping
/// its connection while git carries on. No prompts: nothing can answer
/// one, and a clone waiting on a password or a host key would hang.
pub fn clone_command(url: &str, branch: Option<&str>, path: &str) -> Vec<String> {
    let mut command: Vec<String> = [
        "env",
        "GIT_TERMINAL_PROMPT=0",
        "timeout",
        "--kill-after=10",
        &CLONE_TIMEOUT.as_secs().to_string(),
        "git",
        "-c",
        "core.sshCommand=ssh -o BatchMode=yes",
        "clone",
        "--quiet",
    ]
    .map(str::to_string)
    .to_vec();
    if let Some(branch) = branch {
        command.extend(["--branch".to_string(), branch.to_string()]);
    }
    command.extend(["--".to_string(), url.to_string(), path.to_string()]);
    command
}

/// What to say about a clone that exited with `exit_code` and printed
/// `output`, into `path`.
pub fn clone_failure(exit_code: i32, output: &str, url: &str, path: &str) -> String {
    let output = output.trim();
    // `timeout`'s own exit code for stopping git. 137 (SIGKILL) isn't
    // proof of it: the pod's memory limit kills with SIGKILL too.
    if exit_code == 124 {
        return format!("{CLONE_TIMED_OUT} ({} minutes) cloning {url}.", CLONE_TIMEOUT.as_secs() / 60);
    }
    if exit_code == 137 {
        return format!(
            "git clone {url} was killed (exit code 137): most likely it ran out of memory, \
                 or it hit the {}-minute limit. {output}",
            CLONE_TIMEOUT.as_secs() / 60
        )
        .trim()
        .to_string();
    }
    // Git refuses a directory that already exists and isn't empty, and
    // removes one it made itself when it fails. Only a clone that was
    // cut off (Stop, the timeout, a restart) leaves one behind, which
    // the error says how to deal with.
    if output.contains("already exists") {
        return format!(
            "{output} If it's what's left of an earlier clone that was cut off, delete it \
                 (rm -rf {path}) and clone again, but after a clone that was just stopped, wait \
                 a moment first: it may still be running. Otherwise clone into another directory."
        );
    }
    if output.is_empty() {
        format!("git clone {url} failed (exit code {exit_code})")
    } else {
        output.to_string()
    }
}

/// What a finished clone checked out.
#[derive(Clone, Debug, PartialEq)]
pub struct ClonedRepo {
    /// `None` for an empty repo: nothing committed yet.
    pub commit: Option<String>,
    pub branch: String,
}

/// Clones `url` into `/workspace/<dir>` in the pod, at `branch`
/// or the remote's default. The error is git's own output.
pub async fn clone_into_pod(
    shell: &sandbox::PodShell,
    url: &str,
    branch: Option<&str>,
    dir: &str,
) -> Result<ClonedRepo, String> {
    if parse_remote(url).is_none() {
        return Err(format!("{url} isn't a git URL smelt can clone."));
    }
    let path = format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR);
    let command = clone_command(url, branch, &path);
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let timed_out = || format!("{CLONE_TIMED_OUT} ({} minutes) cloning {url}.", CLONE_TIMEOUT.as_secs() / 60);
    // The pod's own `timeout` stops the clone; smelt waits a little
    // longer, in case the connection itself stalls.
    let clone = tokio::time::timeout(
        CLONE_TIMEOUT + std::time::Duration::from_secs(30),
        shell.run(&command, None),
    )
    .await
    .map_err(|_| timed_out())?
    .map_err(|e| e.to_string())?;
    if clone.exit_code != 0 {
        return Err(clone_failure(clone.exit_code, &format!("{}{}", clone.stdout, clone.stderr), url, &path));
    }
    // The branch from HEAD itself, and the commit only if there is
    // one: a brand-new empty repo has a branch but no commit yet.
    let head = shell
        .run(
            &[
                "sh",
                "-c",
                r#"cd "$1" && git symbolic-ref --short -q HEAD || git rev-parse --abbrev-ref HEAD; git rev-parse --verify -q HEAD || true"#,
                "sh",
                &path,
            ],
            None,
        )
        .await
    .map_err(|e| e.to_string())?;
    let mut lines = head.stdout.lines();
    match (head.exit_code, lines.next()) {
        (0, Some(branch)) if !branch.is_empty() => Ok(ClonedRepo {
            branch: branch.to_string(),
            commit: lines.next().filter(|c| !c.is_empty()).map(str::to_string),
        }),
        _ => Err(format!("cloned, but couldn't read what was checked out: {}{}", head.stdout.trim(), head.stderr.trim())),
    }
}

/// How long a clone may take before smelt gives up on it.
pub(super) const CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A checkout directory is one name under `/workspace`.
pub fn validate_checkout_dir(dir: &str) -> Result<(), String> {
    // A leading dot is fine (`org/.github`); `.` and `..` aren't names.
    let valid = !dir.is_empty()
        && dir.len() <= 100
        && dir != "."
        && dir != ".."
        && !dir.starts_with('-')
        && dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{dir:?} can't be a checkout directory: use one name of letters, digits, \
                 -, _ or ., not starting with -."
        ))
    }
}

/// The checkout's current commit, for labelling a loaded AGENTS.md.
pub(super) async fn head_commit(shell: &sandbox::PodShell, dir: &str) -> Option<String> {
    let path = format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR);
    let head = shell.run(&["git", "-C", &path, "rev-parse", "--verify", "-q", "HEAD"], None)
        .await
        .ok()?;
    (head.exit_code == 0).then(|| head.stdout.trim().to_string())
}

/// Tells every tab watching the conversation what its repos are now.
pub(crate) async fn publish_repos(pool: &PgPool, conversation_id: i64) {
    match list_repos(pool, conversation_id).await {
        Ok(repos) => events::publish(conversation_id, ConversationEvent::ReposUpdate { repos }),
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't list repos to publish"),
    }
}

/// What a clone request comes to, before anything is written.
enum ClonePlan {
    /// The same repo and branch is already checked out.
    Existing(db::ConversationRepo),
    /// Clone into `dir`, retrying `retry`'s failed attempt there if set.
    Clone {
        key: String,
        dir: String,
        retry: Option<i64>,
    },
}

/// Checks a clone request against the conversation's repos.
async fn plan_clone(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    branch: Option<&str>,
    dir: Option<&str>,
) -> Result<ClonePlan, String> {
    let key = remote_key(url).ok_or_else(|| format!("{url} isn't a git URL smelt can clone."))?;
    let named_dir = dir.map(str::trim).filter(|d| !d.is_empty());
    #[expect(clippy::expect_used, reason = "remote_key parsed the URL just above, so it has a default directory")]
    let dir = match named_dir {
        Some(dir) => dir.to_string(),
        None => default_checkout_dir(url).expect("remote_key parsed it"),
    };
    validate_checkout_dir(&dir)?;

    let existing = db::list_conversation_repos(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    // The same repo and branch is already checked out; a directory
    // named on purpose asks for a checkout there, though.
    if let Some(same) = existing.iter().find(|r| {
        r.remote_key == key
            && r.branch.as_deref() == branch
            && r.status != "failed"
            && (named_dir.is_none() || r.dir == dir)
    }) {
        return Ok(ClonePlan::Existing(same.clone()));
    }
    let retry = match existing.iter().find(|r| r.dir == dir) {
        // A failed attempt at this directory is retried in place, with
        // whatever repo is asked for now (a corrected URL, say): git
        // removed what it made, so nothing there is the old repo's.
        Some(failed) if failed.status == "failed" => Some(failed.id),
        Some(other) => {
            return Err(format!(
                "/workspace/{dir} is already used by {}. Pick another directory.",
                other.url
            ));
        }
        None => None,
    };
    Ok(ClonePlan::Clone { key, dir, retry })
}

/// Writes a planned clone down as `cloning`, so a turn waits for it.
pub(super) async fn record_clone(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    branch: Option<&str>,
    key: &str,
    dir: &str,
    retry: Option<i64>,
) -> Result<Option<db::ConversationRepo>, String> {
    let repo = match retry {
        // With what's asked for now, not what failed. Only a failed
        // clone is retried: another request that got there first
        // leaves `None`, for `after_lost_race` (SME-86).
        Some(id) => match db::retry_repo_clone(pool, id, url, key, branch)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(repo) => repo,
            None => return Ok(None),
        },
        // Another request recorded a clone at `dir` since this one
        // was planned: the same race, for a fresh clone.
        None => match db::create_conversation_repo(pool, conversation_id, url, key, branch, dir).await {
            Ok(repo) => repo,
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Ok(None),
            Err(e) => return Err(e.to_string()),
        },
    };
    publish_repos(pool, conversation_id).await;
    Ok(Some(repo))
}

/// Another request retried the failed clone at `dir` first: what this
/// one comes to now. The same repo and branch there (cloning or
/// ready) is this request's checkout too; anything else is refused as
/// a fresh request would be (SME-86 code review).
pub(super) async fn after_lost_race(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    branch: Option<&str>,
    dir: &str,
) -> Result<db::ConversationRepo, String> {
    match plan_clone(pool, conversation_id, url, branch, Some(dir)).await? {
        ClonePlan::Existing(repo) => Ok(repo),
        // The other request's attempt has failed already.
        ClonePlan::Clone { .. } => Err(format!(
            "Another request retried the clone at /workspace/{dir} at the same time, and it failed; see the repo's error, then clone it again."
        )),
    }
}

/// Clones a recorded repo into pod `pod_id`; `guard` marks it
/// interrupted if this is dropped first.
async fn run_clone(
    pool: &PgPool,
    conversation_id: i64,
    pod_id: i64,
    repo: db::ConversationRepo,
    guard: CloneGuard,
) -> Result<RepoSummary, String> {
    let outcome = clone_repo_row(pool, pod_id, &repo, guard).await;
    publish_repos(pool, conversation_id).await;
    outcome?;
    summarise(pool, get_repo(pool, repo.id).await?).await
}

/// Records `url` as one of the conversation's repos and clones it into
/// the conversation's pod, at `/workspace/<dir>` (the repo's name when
/// `dir` is `None`), starting the pod if it has none (SME-49). The same
/// repo and branch already checked out is returned as it is rather than
/// cloned twice.
pub async fn clone_repo(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    branch: Option<&str>,
    dir: Option<&str>,
) -> Result<RepoSummary, String> {
    let url = url.trim();
    let branch = branch.map(str::trim).filter(|b| !b.is_empty());
    let (key, dir, retry) = match plan_clone(pool, conversation_id, url, branch, dir).await? {
        ClonePlan::Existing(repo) => return summarise(pool, repo).await,
        ClonePlan::Clone { key, dir, retry } => (key, dir, retry),
    };
    let Some(repo) = record_clone(pool, conversation_id, url, branch, &key, &dir, retry).await? else {
        let existing = after_lost_race(pool, conversation_id, url, branch, &dir).await?;
        return summarise(pool, existing).await;
    };
    clone_recorded(pool, conversation_id, repo).await
}

/// The user's "Work on a repo": the repo is recorded first, so a
/// message sent while the sandbox starts waits for the clone; then the
/// conversation's sandbox is started if it has none, and it's cloned.
#[cfg(test)]
pub async fn attach_repo(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    branch: Option<&str>,
) -> Result<RepoSummary, String> {
    let (shown, pending, _notices) = start_attach(pool, conversation_id, url, branch, None)
        .await
        .map_err(|failed| failed.error)?;
    Ok(finish_attach(pool, conversation_id, pending).await?.unwrap_or(shown))
}

/// A "Work on a repo" that failed. A failure after the trust committed
/// still carries the notices it owes the waiting models, whose cards it
/// dropped (SME-224 review 1); `notices` is empty when it failed first.
#[derive(Debug)]
pub struct AttachFailed {
    pub error: String,
    pub notices: Vec<(i64, String)>,
}

/// The part of "Work on a repo" that answers the button: records trust
/// and the repo (as cloning, so a message sent meanwhile waits for it).
/// Returns what to show now, the repo `finish_attach` still has to clone
/// (`None`: already checked out), and the notices for the models whose
/// cards the trust dropped. Touches no pod.
pub async fn start_attach(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    branch: Option<&str>,
    dir: Option<&str>,
) -> Result<(RepoSummary, Option<db::ConversationRepo>, Vec<(i64, String)>), AttachFailed> {
    let untrusted = |error| AttachFailed { error, notices: Vec::new() };
    let url = url.trim();
    let branch = branch.map(str::trim).filter(|b| !b.is_empty());
    let plan = plan_clone(pool, conversation_id, url, branch, dir).await.map_err(untrusted)?;
    let key = match &plan {
        // Already checked out (the model cloned it, say): the user
        // naming it trusts it all the same.
        ClonePlan::Existing(repo) => repo.remote_key.clone(),
        ClonePlan::Clone { key, .. } => key.clone(),
    };
    // The user named this repo, so its AGENTS.md is trusted without
    // asking (SME-32's plan, open question 1).
    // Committed before the repo is recorded, outside the lock: the lock
    // covers trust and cards, and a repo recorded as cloning has no card.
    let notices = trust_named_remote(pool, conversation_id, &key, url).await.map_err(untrusted)?;
    match record_attach(pool, conversation_id, url, branch, plan).await {
        Ok((shown, pending)) => Ok((shown, pending, notices)),
        Err(error) => Err(AttachFailed { error, notices }),
    }
}

/// `start_attach`'s step after the trust: records the planned clone.
async fn record_attach(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    branch: Option<&str>,
    plan: ClonePlan,
) -> Result<(RepoSummary, Option<db::ConversationRepo>), String> {
    let (key, dir, retry) = match plan {
        ClonePlan::Existing(repo) => return Ok((summarise(pool, repo).await?, None)),
        ClonePlan::Clone { key, dir, retry } => (key, dir, retry),
    };
    let Some(repo) = record_clone(pool, conversation_id, url, branch, &key, &dir, retry).await? else {
        let existing = after_lost_race(pool, conversation_id, url, branch, &dir).await?;
        return Ok((summarise(pool, existing).await?, None));
    };
    Ok((summarise(pool, repo.clone()).await?, Some(repo)))
}

/// The rest of "Work on a repo", run apart from the button's request:
/// starts the sandbox if needed, and clones `pending`. Returns the
/// cloned repo, or `None` when there was nothing to clone.
pub async fn finish_attach(
    pool: &PgPool,
    conversation_id: i64,
    pending: Option<db::ConversationRepo>,
) -> Result<Option<RepoSummary>, String> {
    let Some(repo) = pending else {
        ensure_sandbox(pool, conversation_id).await?;
        return Ok(None);
    };
    clone_recorded(pool, conversation_id, repo).await.map(Some)
}

/// Clones a repo recorded as `cloning` into the conversation's pod,
/// starting the pod if it has none. A pod that won't start fails the
/// repo with why.
async fn clone_recorded(
    pool: &PgPool,
    conversation_id: i64,
    repo: db::ConversationRepo,
) -> Result<RepoSummary, String> {
    let guard = CloneGuard::new(pool, &repo, conversation_id);
    let pod_id = match ensure_sandbox(pool, conversation_id).await {
        Ok(pod_id) => pod_id,
        Err(e) => {
            guard.finish();
            let told = fail_attempt(pool, &repo, &e).await;
            publish_repos(pool, conversation_id).await;
            return Err(told);
        }
    };
    run_clone(pool, conversation_id, pod_id, repo, guard).await
}

/// The conversation's live pod, started if it has none.
async fn ensure_sandbox(pool: &PgPool, conversation_id: i64) -> Result<i64, String> {
    sandbox::start_or_get_pod(pool, conversation_id)
        .await
        .map_err(|e| format!("Couldn't start the sandbox: {e}"))
}

/// How a clone that ran out of time starts its error.
pub const CLONE_TIMED_OUT: &str = "The clone took too long and was stopped";

/// What an attempt at a clone that a newer attempt replaced is told
/// (SME-86): its result isn't the repo's.
pub const CLONE_SUPERSEDED: &str =
    "This clone was superseded by another attempt at the same directory; see the repo's status for how that went.";

/// What a clone cut off before it finished is marked with.
pub const CLONE_INTERRUPTED: &str =
    "The clone was interrupted (the turn was stopped, or smelt restarted). Clone it again.";

/// Marks a clone failed if it's dropped before `finish`: the turn was
/// stopped, or the request went away, mid-clone.
pub(crate) struct CloneGuard {
    pool: PgPool,
    repo_id: i64,
    /// The attempt it guards: marking it failed can't touch a newer one.
    attempt: i32,
    conversation_id: i64,
    finished: bool,
}

impl CloneGuard {
    pub(crate) fn new(pool: &PgPool, repo: &db::ConversationRepo, conversation_id: i64) -> Self {
        CloneGuard {
            pool: pool.clone(),
            repo_id: repo.id,
            attempt: repo.attempt,
            conversation_id,
            finished: false,
        }
    }

    pub(crate) fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for CloneGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let (pool, repo_id, attempt, conversation_id) =
            (self.pool.clone(), self.repo_id, self.attempt, self.conversation_id);
        tokio::spawn(async move {
            if let Err(e) = db::set_repo_failed(&pool, repo_id, attempt, CLONE_INTERRUPTED).await {
                tracing::warn!(repo_id, error = %e, "couldn't mark an interrupted clone failed");
            }
            publish_repos(&pool, conversation_id).await;
        });
    }
}

/// Records `repo`'s attempt as failed with `error`, and returns what
/// to tell the caller: `error`, or `CLONE_SUPERSEDED` when a newer
/// attempt owns the repo now.
pub(super) async fn fail_attempt(pool: &PgPool, repo: &db::ConversationRepo, error: &str) -> String {
    match db::set_repo_failed(pool, repo.id, repo.attempt, error).await {
        Ok(false) => CLONE_SUPERSEDED.to_string(),
        Ok(true) => error.to_string(),
        Err(e) => {
            tracing::warn!(repo_id = repo.id, error = %e, "couldn't mark a failed clone failed");
            error.to_string()
        }
    }
}

/// Records a finished clone with its AGENTS.md and persona files, and
/// disarms its guard: whatever runs afterwards (reading its AGENTS.md)
/// being cut off doesn't make the clone "interrupted".
pub(super) async fn record_cloned(
    pool: &PgPool,
    repo: &db::ConversationRepo,
    cloned: &ClonedRepo,
    agents_files: Result<AgentsPaths, String>,
    persona_files: Result<Vec<String>, String>,
    guard: CloneGuard,
) -> Result<(), String> {
    // The clone is good even if its files can't be listed; the panel
    // says why.
    let mut errors = Vec::new();
    let files = match agents_files {
        Ok(AgentsPaths { files, left_out }) => {
            match left_out {
                0 => {}
                1 => errors.push("1 AGENTS.md file was left out: its path holds control characters".to_string()),
                n => errors.push(format!("{n} AGENTS.md files were left out: their paths hold control characters")),
            }
            files
        }
        Err(e) => {
            errors.push(format!("Couldn't list its AGENTS.md files: {e}"));
            Vec::new()
        }
    };
    let personas = persona_files.unwrap_or_else(|e| {
        errors.push(format!("Couldn't list its persona files: {e}"));
        Vec::new()
    });
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    let recorded = db::set_repo_ready(
        pool,
        repo.id,
        repo.attempt,
        &cloned.branch,
        cloned.commit.as_deref(),
        &files,
        &personas,
        error.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    guard.finish();
    if recorded { Ok(()) } else { Err(CLONE_SUPERSEDED.to_string()) }
}

async fn clone_repo_row(
    pool: &PgPool,
    pod_id: i64,
    repo: &db::ConversationRepo,
    guard: CloneGuard,
) -> Result<(), String> {
    let outcome = match sandbox::PodShell::for_pod(pool, pod_id).await {
        Ok(shell) => clone_into_pod(&shell, &repo.url, repo.branch.as_deref(), &repo.dir)
            .await
            .map(|cloned| (shell, cloned)),
        Err(e) => Err(e.to_string()),
    };
    match outcome {
        Ok((shell, cloned)) => {
            // What the model can load with load_instructions.
            let files = list_agents_files(&shell, &repo.dir).await.map_err(|e| {
                tracing::warn!(repo = %crate::telemetry::scrub_urls(&repo.url), error = %e, "couldn't list AGENTS.md files");
                e.to_string()
            });
            // What list_personas shows once the user trusts the repo.
            let personas = list_persona_files(&shell, &repo.dir).await.map(|p| p.files).inspect_err(|e| {
                tracing::warn!(repo = %crate::telemetry::scrub_urls(&repo.url), error = %e, "couldn't list persona files");
            });
            record_cloned(pool, repo, &cloned, files, personas, guard).await
        }
        Err(e) => {
            let told = fail_attempt(pool, repo, &e).await;
            guard.finish();
            Err(told)
        }
    }
}
