//! A child agent's checkout (SME-236): which ref it's cloned at, and the
//! clone itself, which takes the remote's default branch and then checks
//! out that ref (docs/design/personas/checkouts-and-volumes.md).

use super::*;

/// What a child's clone checks out after cloning the default branch.
#[derive(Clone, Debug, PartialEq)]
pub enum CloneTarget {
    /// The parent's exact commit, on a branch named as the parent's
    /// upstream branch, tracking it when the remote still has it.
    Commit { sha: String, branch: String },
    /// A branch, tag or commit on the remote, as `start_agent` was given it.
    Ref(String),
}

/// Whether `r` can name a branch, tag or commit on the command line and in
/// `refs/remotes/origin/<r>`: 1 to 255 bytes, no control characters or
/// whitespace, not starting with `-`, no `..` or `@{`, not ending in
/// `.lock` or `/`. Never passed through a shell string.
pub fn validate_ref(r: &str) -> Result<(), String> {
    let valid = !r.is_empty()
        && r.len() <= 255
        && !r.starts_with('-')
        && !r.ends_with('/')
        && !r.ends_with(".lock")
        && !r.contains("..")
        && !r.contains("@{")
        && !r.chars().any(|c| c.is_control() || c.is_whitespace());
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{:?} can't be a ref: give a branch, tag or commit of 1 to 255 characters, without \
             whitespace, control characters, .. or @{{, not starting with - or ending in / or .lock.",
            r
        ))
    }
}

/// Whether `r` reads as a commit id, whole or abbreviated.
fn is_hex_id(r: &str) -> bool {
    (4..=64).contains(&r.len()) && r.chars().all(|c| c.is_ascii_hexdigit())
}

/// The branch a ref asks for, for the child's repo row: none for a commit id.
pub fn requested_branch(target: &CloneTarget) -> Option<String> {
    match target {
        CloneTarget::Commit { branch, .. } => Some(branch.clone()),
        CloneTarget::Ref(r) if is_hex_id(r) => None,
        CloneTarget::Ref(r) => Some(r.clone()),
    }
}

/// The commit the checkout `/workspace/<dir>` is on and its upstream
/// branch's name (without the remote): what a child started without a ref
/// is cloned at. Fetches first, under `timeout`, and refuses a detached
/// HEAD, a branch with no upstream, commits not on the upstream and
/// uncommitted changes to tracked files, each saying what to do. An
/// untracked file is no reason to refuse: the child doesn't get it either way.
pub async fn parent_commit(shell: &sandbox::PodShell, dir: &str) -> Result<(String, String), String> {
    let path = format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR);
    // The upstream must be a branch on `origin`, the remote the child
    // clones (review 1): one on another remote, or a local branch, would be
    // checked against a ref this fetch doesn't update.
    let script = r#"cd "$1" || exit 1
        GIT_TERMINAL_PROMPT=0 timeout 60 git -c core.sshCommand='ssh -o BatchMode=yes' fetch --quiet origin || exit 10
        b=$(git symbolic-ref --short -q HEAD) || exit 11
        echo "$b"
        u=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null) || exit 12
        echo "$u"
        m=$(git config "branch.$b.merge") || exit 12
        echo "${m#refs/heads/}"
        [ "$(git config "branch.$b.remote")" = origin ] && [ "$m" != "${m#refs/heads/}" ] || exit 15
        git merge-base --is-ancestor HEAD '@{u}' || { git rev-list --count '@{u}..HEAD'; exit 13; }
        [ -z "$(git status --porcelain --untracked-files=no)" ] || exit 14
        git rev-parse HEAD"#;
    let ran = shell.run(&["sh", "-c", script, "sh", &path], None).await.map_err(|e| e.to_string())?;
    let lines: Vec<&str> = ran.stdout.lines().collect();
    let line = |i: usize| lines.get(i).copied().unwrap_or("").trim().to_string();
    let (branch, upstream, name) = (line(0), line(1), line(2));
    let or_ref = "or pass ref to start the agent on a branch, tag or commit of the remote";
    match ran.exit_code {
        0 => Ok((line(3), name)),
        10 => Err(format!("couldn't fetch {path}'s remote to check your commit is on it: {}", ran.stderr.trim())),
        11 => Err(format!("{path} is on a detached HEAD: check out a branch, commit and push, {or_ref}")),
        12 => Err(format!("{path}'s branch {branch} has no upstream: push it (git push -u origin {branch}), {or_ref}")),
        13 => Err(format!("{path} has {} commits not on {upstream}: push them, {or_ref}", line(3))),
        14 => Err(format!("{path} has uncommitted changes to tracked files: commit and push them, {or_ref}")),
        15 => Err(format!(
            "{path}'s branch {branch} tracks {upstream}, not a branch on origin: push it (git push -u origin {branch}), {or_ref}"
        )),
        code => Err(format!("couldn't read {path}'s commit (exit code {code}): {}", ran.stderr.trim())),
    }
}

/// Exit code of `checkout_ref`'s script for a ref the remote doesn't have.
const NO_SUCH_REF: i32 = 3;

/// Checks out `target` in the checkout at `/workspace/<dir>`, just cloned
/// from `url` at its default branch. A branch is checked out tracking the
/// remote's; a tag or a commit id detached; a commit missing from the
/// clone (one no ref reaches) is fetched by id, which protocol v2 allows
/// for any object the server has.
pub async fn checkout_ref(shell: &sandbox::PodShell, dir: &str, url: &str, target: &CloneTarget) -> Result<(), String> {
    let path = format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR);
    let fetch = "fetch_id() { git cat-file -e \"$1^{commit}\" 2>/dev/null || \
                 GIT_TERMINAL_PROMPT=0 timeout 120 git -c core.sshCommand='ssh -o BatchMode=yes' fetch --quiet origin \"$1\"; }";
    let (script, args): (String, Vec<&str>) = match target {
        CloneTarget::Commit { sha, branch } => (
            format!(
                "set -e; cd \"$1\"; {fetch}
                 fetch_id \"$2\"
                 git checkout -q -B \"$3\" \"$2\"
                 git branch -q --set-upstream-to=\"origin/$3\" \"$3\" 2>/dev/null || true"
            ),
            vec![sha.as_str(), branch.as_str()],
        ),
        CloneTarget::Ref(r) => (
            format!(
                "cd \"$1\" || exit 1; r=\"$2\"; {fetch}
                 if git show-ref --verify -q \"refs/remotes/origin/$r\"; then
                     exec git checkout -q -B \"$r\" --track \"origin/$r\"
                 elif git show-ref --verify -q \"refs/tags/$r\"; then
                     exec git checkout -q --detach \"refs/tags/$r\"
                 fi
                 case \"$r\" in *[!0-9a-fA-F]*) exit {NO_SUCH_REF};; esac
                 if [ ${{#r}} -ge 40 ]; then fetch_id \"$r\" || exit {NO_SUCH_REF}; fi
                 git cat-file -e \"$r^{{commit}}\" 2>/dev/null || exit {NO_SUCH_REF}
                 exec git checkout -q --detach \"$r\""
            ),
            vec![r.as_str()],
        ),
    };
    let mut command = vec!["sh", "-c", script.as_str(), "sh", path.as_str()];
    command.extend(args);
    let ran = shell.run(&command, None).await.map_err(|e| e.to_string())?;
    match ran.exit_code {
        0 => Ok(()),
        NO_SUCH_REF => Err(match target {
            CloneTarget::Ref(r) => format!("no branch, tag or commit `{r}` on {url}"),
            CloneTarget::Commit { sha, .. } => format!("no commit {sha} on {url}"),
        }),
        code => Err(format!(
            "couldn't check out {} in {path} (exit code {code}): {}",
            match target {
                CloneTarget::Ref(r) => r.as_str(),
                CloneTarget::Commit { sha, .. } => sha.as_str(),
            },
            format!("{}{}", ran.stdout, ran.stderr).trim()
        )),
    }
}

/// A child's clone: the repo recorded as `cloning` in `start_agent`'s
/// transaction, cloned into the child's pod (started if it has none) at
/// the remote's default branch, then checked out at `target`, and recorded
/// ready with what that checkout holds, its AGENTS.md and persona files
/// included. The repo is failed with the error otherwise, as
/// `clone_recorded`'s are.
pub(crate) async fn clone_child_repo(
    pool: &PgPool,
    child: i64,
    repo: &db::ConversationRepo,
    target: &CloneTarget,
) -> Result<ClonedRepo, String> {
    let guard = CloneGuard::new(pool, repo, child);
    let cloned = async {
        let pod_id = ensure_sandbox(pool, child).await?;
        let shell = sandbox::PodShell::for_pod(pool, pod_id).await.map_err(|e| e.to_string())?;
        clone_into_pod(&shell, &repo.url, None, &repo.dir).await?;
        checkout_ref(&shell, &repo.dir, &repo.url, target).await?;
        let path = format!("{}/{}", crate::sandbox::WORKSPACE_DIR, repo.dir);
        let cloned = checked_out(&shell, &path).await?;
        Ok::<_, String>((shell, cloned))
    }
    .await;
    let outcome = match cloned {
        Ok((shell, cloned)) => {
            let files = list_agents_files(&shell, &repo.dir).await;
            let personas = list_persona_files(&shell, &repo.dir).await.map(|p| p.files);
            record_cloned(pool, repo, &cloned, files, personas, guard).await.map(|()| cloned)
        }
        Err(e) => {
            let told = fail_attempt(pool, repo, &e).await;
            guard.finish();
            Err(told)
        }
    };
    publish_repos(pool, child).await;
    outcome
}
