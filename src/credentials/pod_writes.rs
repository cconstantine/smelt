//! The commands an install runs in a pod (SME-440): each file written
//! beside its target and renamed over it, each credential directory pruned
//! to the names kept, and every exec that goes unanswered run again.

use super::PodFile;
use crate::sandbox;

/// Writes `files` into the pod's sandbox container, then prunes each of
/// `prune_dirs` to the names kept in `files`, so a file deleted since the
/// last install goes too.
pub async fn install_pod_files(
    shell: &sandbox::PodShell,
    files: &[PodFile],
    prune_dirs: &[&str],
) -> Result<(), sandbox::SandboxError> {
    use sandbox::SandboxError;
    for dir in prune_dirs {
        let made = run_answered(shell, &["sh", "-c", r#"mkdir -p -m 700 "$1""#, "sh", dir], None).await?;
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
        let written = run_answered(
            shell,
            &[
                "sh",
                "-c",
                WRITE_FILE_SCRIPT,
                "sh",
                &file.path,
                &mode,
                &length,
            ],
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
               [ -n "$keep" ] || rm -f -- "$f"
           done"#,
            "sh",
            dir,
        ];
        prune.extend(keep);
        let pruned = run_answered(shell, &prune, None).await?;
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
chmod "$2" "$t" && mv "$t" "$1" || { rm -f "$t"; exit 1; }"#;

/// How long one of an install's execs may go unanswered before it counts as
/// stalled and is run again (SME-440). On a loaded CI runner one exec of a
/// reinstall ran and never answered; with nothing bounding it but the whole
/// install's `INSTALL_TIMEOUT`, the install failed and the pod kept a key
/// and a secret its conversation no longer had — in production, a live pod
/// keeping what a narrowing or a delete took away, or a new pod failing to
/// start. Every install exec answers in about a second, 1.3 s under SME-341's
/// load, so 10 s is well past a slow one, and leaves `INSTALL_TIMEOUT` room
/// for several tries.
pub(super) const EXEC_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// `shell.run`, run again whenever it goes `EXEC_STALL_AFTER` without an
/// answer. Safe because every install command is idempotent: `mkdir -p`, a
/// write beside the target renamed over it, a prune to the same names. A
/// stalled exec may still finish in the pod after this has moved on, which
/// lands the same bytes (SME-410 is the case where a later install's change
/// is in between). Bounded by the caller: `write_pod_files` holds the whole
/// install to `INSTALL_TIMEOUT`.
async fn run_answered(
    shell: &sandbox::PodShell,
    command: &[&str],
    stdin: Option<&[u8]>,
) -> Result<sandbox::ExecResult, sandbox::SandboxError> {
    loop {
        match tokio::time::timeout(EXEC_STALL_AFTER, shell.run(command, stdin)).await {
            Ok(ran) => return ran,
            // The command's arguments name a path, never a value: file
            // contents go in on stdin, which isn't logged.
            Err(_) => tracing::warn!(
                after = ?EXEC_STALL_AFTER,
                target = command.get(4).copied().unwrap_or(""),
                "an install's exec stalled; running it again"
            ),
        }
    }
}
