//! A persona refresh's read of the remote itself (SME-516, review 1 H1):
//! smelt fetches the conversation's recorded remote (`conversation_repos.url`)
//! into a private directory of its own, never through the seat's pod. Every
//! input the old in-pod read trusted (`origin`'s URL, `origin/HEAD`, the refs,
//! the `git` on PATH) was the seat's to forge; here the URL comes from the
//! database and is checked against the trusted remote's key, the default
//! branch is the remote's own `HEAD` (`ls-remote --symref`), and the ancestry
//! check and the read run in a bare repository only this call can write.

use super::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long each git command of the read may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The server image's pinned host keys of the common git hosts
/// (docker/known_hosts, copied by the Dockerfile's `runtime` stage). An ssh
/// remote on any other host can't be refreshed; an https one can.
pub const KNOWN_HOSTS: &str = "/etc/smelt/known_hosts";

/// The transports a refresh may fetch over: ssh (with the stored keys) and
/// https. A local path or `file://` would read smelt's own disk, so only
/// tests (whose fixtures are local) add `file`.
fn allowed_protocols() -> &'static str {
    if cfg!(test) { "ssh:https:file" } else { "ssh:https" }
}

/// Whether `url` names a transport `allowed_protocols` lets through.
fn transport_allowed(url: &str) -> bool {
    match url.split_once("://") {
        Some((scheme, _)) => allowed_protocols().split(':').any(|p| p == scheme),
        // scp-like `[user@]host:path` is ssh (`remote_key` refused the rest).
        None => true,
    }
}

/// The string git is given for `url`: the one `remote_key` keyed (review 2
/// M1). A `scheme://` URL is the `url` crate's own serialization of what it
/// parsed, so git connects to the host and path the key names; an scp-like
/// one is the trimmed text `parse_remote` split.
pub(super) fn fetch_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.contains("://") {
        url::Url::parse(url).ok().map(|parsed| parsed.as_str().to_string())
    } else {
        Some(url.to_string())
    }
}

/// A refresh's `commit`: only a commit id, whole or abbreviated (review 1
/// L1). A ref name would resolve against refs the read doesn't need.
pub fn validate_refresh_commit(commit: &str) -> Result<(), String> {
    if is_hex_id(commit) {
        Ok(())
    } else {
        Err(format!("{commit:?} isn't a commit id: a refresh takes 4 to 64 hex digits, or none for the default branch's tip"))
    }
}

/// The file `rel` at `commit` (the default branch's tip when `None`) on the
/// remote `url`, and that commit's full id. `url` must be the remote whose
/// trust was read (`remote_key`), the commit an ancestor of the remote's
/// default branch, and the file a regular blob within the persona ceiling.
/// `keys` are the stored SSH keys `(name, private key)`, offered for an ssh
/// remote; they're written only into this call's own directory, mode 0600,
/// and removed with it.
pub async fn persona_at_remote(
    url: &str,
    trusted_key: &str,
    rel: &str,
    commit: Option<&str>,
    keys: &[(String, String)],
) -> Result<(String, String), String> {
    if remote_key(url).as_deref() != Some(trusted_key) {
        return Err(format!("the checkout's remote {url} isn't {trusted_key}, the remote whose trust was read; nothing was read"));
    }
    if !transport_allowed(url) {
        return Err(format!("a refresh reads a remote only over ssh or https, and {url} is neither"));
    }
    // From here on git sees only the form that was keyed (review 2 M1).
    let fetch = fetch_url(url).ok_or_else(|| format!("couldn't read {url} as a URL; nothing was read"))?;
    let url = fetch.as_str();
    if let Some(c) = commit {
        validate_refresh_commit(c)?;
    }
    let work = WorkDir::new().map_err(|e| format!("couldn't make a directory to read the persona in: {e}"))?;
    let git = Git::new(&work.0, keys).map_err(|e| format!("couldn't set up the persona's read: {e}"))?;

    let heads = git.run(None, &["ls-remote", "--symref", "--", url, "HEAD"]).await?;
    if heads.code != 0 {
        return Err(format!("couldn't read {url}'s default branch: {}", heads.stderr.trim()));
    }
    let branch = default_branch(&heads.stdout).ok_or_else(|| format!("{url} names no default branch (HEAD) to read the persona at"))?;
    let repo = work.0.join("repo.git");
    let repo_arg = repo.to_string_lossy().to_string();
    let init = git.run(None, &["init", "--bare", "--quiet", &repo_arg]).await?;
    if init.code != 0 {
        return Err(format!("couldn't make a repository to read the persona in: {}", init.stderr.trim()));
    }
    let refspec = format!("+refs/heads/{branch}:{DEFAULT_REF}");
    let fetched = git.run(Some(&repo), &["fetch", "--quiet", "--no-tags", "--", url, &refspec]).await?;
    if fetched.code != 0 {
        return Err(format!("couldn't fetch {url}'s {branch} to read the persona: {}", fetched.stderr.trim()));
    }

    let on = format!("{branch} at {url}");
    let asked = commit.unwrap_or(DEFAULT_REF);
    let resolved = git.run(Some(&repo), &["rev-parse", "--quiet", "--verify", &format!("{asked}^{{commit}}")]).await?;
    let sha = resolved.stdout.trim().to_string();
    if resolved.code != 0 || sha.is_empty() {
        // Only the default branch was fetched: anything else isn't on it.
        return Err(format!("{asked} isn't on {on}: a refresh reads only commits on the remote's default branch"));
    }
    let ancestor = git.run(Some(&repo), &["merge-base", "--is-ancestor", &sha, DEFAULT_REF]).await?;
    if ancestor.code != 0 {
        return Err(format!("{sha} isn't on {on}: a refresh reads only commits on the remote's default branch"));
    }
    let listed = git.run(Some(&repo), &["ls-tree", &sha, "--", rel]).await?;
    let mode = listed.stdout.split(' ').next().unwrap_or("").to_string();
    if listed.code != 0 || mode.is_empty() {
        return Err(format!("no file {rel} on {on} at {sha}"));
    }
    if mode != "100644" && mode != "100755" {
        return Err(format!("{rel} at {sha} isn't a regular file (a symlink is refused)"));
    }
    let object = format!("{sha}:{rel}");
    let size = git.run(Some(&repo), &["cat-file", "-s", &object]).await?;
    let bytes: u64 = size.stdout.trim().parse().map_err(|_| format!("couldn't read {rel}'s size at {sha}"))?;
    let max = crate::git::INSTRUCTIONS_MAX_BYTES;
    if bytes > max as u64 {
        return Err(format!("{rel} at {sha} is {bytes} bytes: a persona file may be at most {max}."));
    }
    let shown = git.run(Some(&repo), &["cat-file", "blob", &object]).await?;
    if shown.code != 0 {
        return Err(format!("couldn't read {rel} at {sha}: {}", shown.stderr.trim()));
    }
    Ok((sha, shown.stdout))
}

/// Where the fetched default branch lands in the private repository.
const DEFAULT_REF: &str = "refs/smelt/default";

/// The branch `ls-remote --symref <url> HEAD` names (`ref: refs/heads/<b>\tHEAD`),
/// if it can be a branch name on a command line and in a refspec.
pub(super) fn default_branch(listing: &str) -> Option<String> {
    let line = listing.lines().find_map(|l| l.strip_prefix("ref: "))?;
    let (target, name) = line.split_once('\t')?;
    let branch = target.strip_prefix("refs/heads/")?;
    let plain = !branch.contains([':', '*', '?', '[', '\\', '^', '~']);
    (name == "HEAD" && plain && validate_ref(branch).is_ok()).then(|| branch.to_string())
}

/// A directory of this read's own, removed when it's dropped.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("smelt-persona-read-{}-{nanos}-{n}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(path = %self.0.display(), error = %e, "couldn't remove a persona read's directory");
        }
    }
}

/// A command's exit code and output.
struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

/// git with no config but its own: no system or global config, a home of
/// its own, no prompts, only `allowed_protocols`, and ssh offering only the
/// stored keys, never an agent's or the server user's.
struct Git {
    env: Vec<(String, String)>,
}

impl Git {
    fn new(work: &Path, keys: &[(String, String)]) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
        // Host keys only from the image's pinned list, never trusted on
        // first use: the read's answer decides a seat's grant.
        let mut ssh = format!(
            "ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -o ConnectTimeout=30 \
             -o StrictHostKeyChecking=yes -o UserKnownHostsFile=/dev/null -o GlobalKnownHostsFile={}",
            quote(Path::new(KNOWN_HOSTS))
        );
        for (i, (_, private_key)) in keys.iter().enumerate() {
            let path = work.join(format!("key{i}"));
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            file.write_all(private_key.as_bytes())?;
            if !private_key.ends_with('\n') {
                file.write_all(b"\n")?;
            }
            ssh.push_str(&format!(" -i {}", quote(&path)));
        }
        let env = [
            ("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())),
            ("HOME", work.to_string_lossy().to_string()),
            ("LC_ALL", "C".to_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".to_string()),
            ("GIT_TERMINAL_PROMPT", "0".to_string()),
            ("GIT_ALLOW_PROTOCOL", allowed_protocols().to_string()),
            ("GIT_SSH_COMMAND", ssh),
        ];
        Ok(Self { env: env.into_iter().map(|(k, v)| (k.to_string(), v)).collect() })
    }

    /// Runs `git <args>` (in the bare repository `dir` when given), bounded
    /// by `GIT_TIMEOUT`.
    async fn run(&self, dir: Option<&Path>, args: &[&str]) -> Result<Ran, String> {
        let mut command = tokio::process::Command::new("git");
        command.env_clear().envs(self.env.iter().map(|(k, v)| (k, v))).stdin(std::process::Stdio::null()).kill_on_drop(true);
        if let Some(dir) = dir {
            command.arg("--git-dir").arg(dir);
        }
        command.args(args);
        let what = args.first().copied().unwrap_or("git");
        let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
            .await
            .map_err(|_| format!("git {what} took over {}s reading the persona's remote", GIT_TIMEOUT.as_secs()))?
            .map_err(|e| format!("couldn't run git to read the persona's remote: {e}"))?;
        Ok(Ran {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).map_err(|_| format!("git {what}'s output isn't UTF-8"))?,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}
