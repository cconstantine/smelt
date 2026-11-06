//! The commands an install runs in a pod (SME-440): each file written
//! beside its target and renamed over it, each credential directory pruned
//! to the names kept, and every exec that goes unanswered run again.

use super::{PodFile, INSTALL_TIMEOUT};
use crate::sandbox;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Writes `files` into the pod's sandbox container, then prunes each of
/// `prune_dirs` to the names kept in `files`, so a file deleted since the
/// last install goes too. Returns only once no exec it started can still
/// change the pod (`Attempts::settle`): the caller's install lock is held
/// until then.
pub async fn install_pod_files(
    shell: &sandbox::PodShell,
    files: &[PodFile],
    prune_dirs: &[&str],
) -> Result<(), sandbox::SandboxError> {
    let mut attempts = Attempts::new();
    let written = write_and_prune(&mut attempts, shell, files, prune_dirs).await;
    attempts.settle().await;
    written
}

async fn write_and_prune(
    attempts: &mut Attempts,
    shell: &sandbox::PodShell,
    files: &[PodFile],
    prune_dirs: &[&str],
) -> Result<(), sandbox::SandboxError> {
    use sandbox::SandboxError;
    for dir in prune_dirs {
        let made = attempts.run(shell, &["sh", "-c", r#"mkdir -p -m 700 "$1""#, "sh", dir], None).await?;
        if made.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't make {dir}: {}",
                made.stderr.trim()
            )));
        }
    }
    // Never emptied: a clone or push running during a reinstall must
    // still find its key. Files are written over in place, and only the
    // ones no longer in `files` removed afterwards.
    for file in files {
        // Written beside the target and renamed over it, so ssh or git
        // never reads half a file; umask keeps a key private from its
        // first byte.
        // The byte count is checked before the rename: an exec dropped
        // mid-stdin (`run_answered` moving past a stall, or the install's
        // own deadline) closes the command's stdin early, and a short
        // file must never be renamed over a good one (SME-440, SME-410).
        let mode = format!("{:o}", file.mode);
        let length = file.content.len().to_string();
        let written = attempts
            .run(
                shell,
                &["sh", "-c", WRITE_FILE_SCRIPT, "sh", &file.path, &mode, &length],
                Some(file.content.as_bytes()),
            )
            .await?;
        if written.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't write {}: {}",
                file.path,
                written.stderr.trim()
            )));
        }
    }
    for dir in prune_dirs {
        let kept_prefix = format!("{dir}/");
        let keep: Vec<&str> = files
            .iter()
            .filter_map(|f| f.path.strip_prefix(&kept_prefix))
            .collect();
        let mut prune = vec![
            "sh",
            "-c",
            r#"cd "$1" && shift && for f in * .[!.]*; do
               [ -e "$f" ] || continue
               keep=; for k in "$@"; do [ "$f" = "$k" ] && keep=1; done
               [ -n "$keep" ] && continue
               [ "$(date +%s)" -lt "${SMELT_INSTALL_DEADLINE:-0}" ] || { echo "past the install's deadline" >&2; exit 1; }
               rm -f -- "$f"
           done"#,
            "sh",
            dir,
        ];
        prune.extend(keep);
        let pruned = attempts.run(shell, &prune, None).await?;
        if pruned.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't remove deleted files from {dir}: {}",
                pruned.stderr.trim()
            )));
        }
    }
    Ok(())
}

/// Writes stdin to `$1` at mode `$2`, only if it is `$3` bytes: beside the
/// target first, then renamed over it. The temp name is this attempt's own
/// (`$$`, unique in the pod while it runs): an abandoned attempt and a later
/// writer sharing one name wrote into one inode, so one's bytes could land
/// inside the other's renamed file (SME-440 review M1). The prune sweeps a
/// leftover temp file, since no kept name matches it. A short write
/// removes its own temp file and says why (review L2): the install stops
/// before its prune, which would otherwise leave a partial key behind.
pub(crate) const WRITE_FILE_SCRIPT: &str = r#"umask 077
t="$1.new.$$"
cat > "$t" || { rm -f "$t"; echo "couldn't write $t" >&2; exit 1; }
n=$(wc -c < "$t")
if [ "$n" -ne "$3" ]; then rm -f "$t"; echo "short write to $1: $n of $3 bytes" >&2; exit 1; fi
chmod "$2" "$t" || { rm -f "$t"; exit 1; }
[ "$(date +%s)" -lt "${SMELT_INSTALL_DEADLINE:-0}" ] || { rm -f "$t"; echo "past the install's deadline" >&2; exit 1; }
mv "$t" "$1" || { rm -f "$t"; exit 1; }"#;

/// How long one of an install's execs may go unanswered before it counts as
/// stalled and is run again (SME-440). On a loaded CI runner one exec of a
/// reinstall ran and never answered; with nothing bounding it but the whole
/// install's `INSTALL_TIMEOUT`, the install failed and the pod kept a key
/// and a secret its conversation no longer had — in production, a live pod
/// keeping what a narrowing or a delete took away, or a new pod failing to
/// start. Every install exec answers in about a second, 1.3 s under SME-341's
/// load, so 10 s is well past a slow one, and leaves `INSTALL_TIMEOUT` room
/// for several tries.
pub(crate) const EXEC_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// How far a node's clock may be behind the server's, for the deadlines
/// below (SME-440 review H1). The server sets each attempt's deadline by
/// its own clock and the pod checks it by its node's. One cluster, its
/// nodes NTP-synced (and in the dev stack and CI, containers on one host's
/// clock), so they agree to well under a second; 5 s also covers a node
/// whose sync has drifted, and a command descheduled for a few seconds
/// between its deadline check and its `mv` or `rm`. It's paid only after
/// an attempt was abandoned.
pub(crate) const CLOCK_SKEW_MARGIN: Duration = Duration::from_secs(5);

/// One install's execs (SME-440 review H1). Dropping an exec's stream
/// doesn't stop its command in the pod, which can even start after the
/// install has moved on. So every attempt carries an absolute deadline,
/// `SMELT_INSTALL_DEADLINE` (Unix seconds), which the write and prune
/// scripts check right before each effect, and runs under the pod's own
/// `timeout`. An attempt unanswered at its deadline is abandoned and run
/// again, and `settle` holds the install until every abandoned attempt's
/// deadline, plus `CLOCK_SKEW_MARGIN`, has passed. The invariant: once an
/// install returns, and its lock is released, nothing it started can
/// change the pod. Every attempt of one install carries the decision read
/// once under that lock, so one landing before its deadline writes what
/// the install meant to.
struct Attempts {
    ends_at: tokio::time::Instant,
    /// The latest deadline of an attempt this install abandoned.
    abandoned_until: Option<SystemTime>,
}

impl Attempts {
    fn new() -> Self {
        Self { ends_at: tokio::time::Instant::now() + INSTALL_TIMEOUT, abandoned_until: None }
    }

    /// Runs `command` until an attempt answers, each under its deadline.
    async fn run(
        &mut self,
        shell: &sandbox::PodShell,
        command: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<sandbox::ExecResult, sandbox::SandboxError> {
        loop {
            let limit = EXEC_STALL_AFTER.min(self.ends_at.saturating_duration_since(tokio::time::Instant::now()));
            if limit < Duration::from_secs(1) {
                return Err(sandbox::SandboxError::GitSetup(format!(
                    "writing credential files into the pod took over {}s",
                    INSTALL_TIMEOUT.as_secs()
                )));
            }
            let deadline = SystemTime::now() + limit;
            // Rounded down, so the pod stops acting no later than this
            // attempt is abandoned.
            let epoch = deadline.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let deadline_var = format!("SMELT_INSTALL_DEADLINE={epoch}");
            let seconds = limit.as_secs().to_string();
            let mut bounded = vec!["env", deadline_var.as_str(), "timeout", "-k", "1", seconds.as_str()];
            bounded.extend_from_slice(command);
            match tokio::time::timeout(limit, shell.run(&bounded, stdin)).await {
                Ok(ran) => return ran,
                Err(_) => {
                    self.abandoned_until = Some(self.abandoned_until.map_or(deadline, |until| until.max(deadline)));
                    // The arguments name a path, never a value: file
                    // contents go in on stdin, which isn't logged.
                    tracing::warn!(
                        after = ?limit,
                        target = command.get(4).copied().unwrap_or(""),
                        "an install's exec went unanswered; running it again"
                    );
                }
            }
        }
    }

    /// Waits until no abandoned attempt can still act in the pod.
    async fn settle(self) {
        if let Some(until) = self.abandoned_until {
            let wait = (until + CLOCK_SKEW_MARGIN).duration_since(SystemTime::now()).unwrap_or_default();
            tokio::time::sleep(wait).await;
        }
    }
}
