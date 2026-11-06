//! Files in the server's pod: reading and writing them over `pods/exec`,
//! the hashes `refresh_open_files` compares the server's open files to,
//! and the project-root lookup. Part of `manager` (moved verbatim in the
//! SME-426 split); `use pod_files::*;` there keeps these names in scope.

use super::*;

/// `text`, read from `path` with one byte past `MAX_FILE_BYTES` asked
/// for, if it fits: a larger file isn't sent cut short.
pub(super) fn within_limit(path: &str, text: String) -> Result<String, String> {
    if text.len() > MAX_FILE_BYTES {
        return Err(format!("{path} is over 1 MiB; language servers aren't sent files that large."));
    }
    Ok(text)
}

/// `path`'s contents, read in the server's pod.
pub(super) async fn read_in_pod(client: &kube::Client, pod_name: &str, path: &str) -> Result<String, String> {
    let limit = (MAX_FILE_BYTES + 1).to_string();
    let result = crate::sandbox::exec_with(client, pod_name, "server", &["head", "-c", &limit, "--", path], None)
        .await
        .map_err(|e| e.to_string())?;
    if result.exit_code != 0 {
        return Err(format!("Couldn't read {path}: {}", result.stderr.trim()));
    }
    within_limit(path, result.stdout)
}

/// Replaces `path` with `content` in the server's pod, if it still has the
/// SHA-256 `expected`: a file changed since it was read isn't overwritten.
pub(super) async fn write_in_pod(client: &kube::Client, pod_name: &str, path: &str, content: &str, expected: &str) -> Result<(), String> {
    let script = r#"[ "$(sha256sum -- "$1" | cut -d' ' -f1)" = "$2" ] || { echo "$1 changed while it was being renamed" >&2; exit 3; }; cat > "$1""#;
    let result = crate::sandbox::exec_with(client, pod_name, "server", &["sh", "-c", script, "_", path, expected], Some(content.as_bytes()))
        .await
        .map_err(|e| e.to_string())?;
    if result.exit_code != 0 {
        return Err(result.stderr.trim().to_string());
    }
    Ok(())
}

pub(super) fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// What to do with each open file, given the SHA-256 of each on disk now
/// (`-` for one that's gone), in the same order: the files to sync again,
/// and the files to close. `None` when the hashes don't line up with the
/// files (the exec printed something else).
pub(super) fn stale_files(open: &[(String, String)], hashes: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    if open.len() != hashes.len() {
        return None;
    }
    let (mut resync, mut close) = (Vec::new(), Vec::new());
    for ((path, synced), hash) in open.iter().zip(hashes) {
        if hash == "-" {
            close.push(path.clone());
        } else if *hash != sha256(synced) {
            resync.push(path.clone());
        }
    }
    Some((resync, close))
}

/// The files a rename touches whose text on disk (`current`) isn't what
/// the server was told (`synced`, `None` if never): the rename's positions
/// would be for other text.
pub(super) fn rename_check(touched: &[(String, String, Option<String>)]) -> Vec<String> {
    touched
        .iter()
        .filter(|(_, current, synced)| synced.as_deref() != Some(current.as_str()))
        .map(|(path, ..)| path.clone())
        .collect()
}

/// Brings the server's open files up to date with the disk (SME-172): once
/// a file is open, the server stops reading it from disk, so one changed
/// by anything but smelt (a terminal) would stay stale. One exec hashes
/// them all; a changed file is synced again, and a deleted one (or one
/// that can no longer be read for the server) is closed, so the server
/// goes back to the disk. An exec that fails skips this, as before; a
/// notification that fails means the session is broken.
pub(super) async fn refresh_open_files(client: &kube::Client, pod_name: &str, session: &Session) -> Result<(), LspError> {
    let open = session.open_documents();
    if open.is_empty() {
        return Ok(());
    }
    // One line per file, in order, so no path is parsed out of the output.
    let script = r#"for f; do if [ -f "$f" ] && h=$(sha256sum < "$f"); then echo "${h%% *}"; else echo -; fi; done"#;
    let mut argv = vec!["sh", "-c", script, "_"];
    argv.extend(open.iter().map(|(path, _)| path.as_str()));
    let hashes = match crate::sandbox::exec_with(client, pod_name, "server", &argv, None).await {
        Ok(result) if result.exit_code == 0 => result.stdout.lines().map(str::to_string).collect::<Vec<_>>(),
        Ok(result) => {
            tracing::info!(pod_name, stderr = %result.stderr.trim(), "couldn't check a language server's open files");
            return Ok(());
        }
        Err(e) => {
            tracing::info!(pod_name, error = %e, "couldn't check a language server's open files");
            return Ok(());
        }
    };
    let Some((resync, mut close)) = stale_files(&open, &hashes) else {
        tracing::info!(pod_name, files = open.len(), lines = hashes.len(), "a language server's open files didn't hash one line each");
        return Ok(());
    };
    for path in resync {
        match read_in_pod(client, pod_name, &path).await {
            Ok(text) => {
                session.sync(&path, &text).await?;
            }
            Err(_) => close.push(path),
        }
    }
    for path in close {
        session.close(&path).await?;
    }
    Ok(())
}

/// Where each of `markers` would be, in every directory from `path`'s up
/// to `/workspace`.
pub(super) fn marker_candidates(path: &str, markers: &[String]) -> Vec<String> {
    let mut candidates = Vec::new();
    for dir in std::path::Path::new(path).ancestors().skip(1) {
        let dir = dir.to_string_lossy();
        if dir != session::WORKSPACE && !session::in_workspace(&dir) {
            break;
        }
        candidates.extend(markers.iter().map(|marker| format!("{dir}/{marker}")));
    }
    candidates
}

/// The project root for `path` among `markers`, checked in the pod.
pub(super) async fn root_in_pod(client: &kube::Client, pod_name: &str, path: &str, markers: &[String]) -> String {
    if markers.is_empty() {
        return session::WORKSPACE.to_string();
    }
    let candidates = marker_candidates(path, markers);
    let mut argv = vec!["sh", "-c", r#"for f; do [ -e "$f" ] && echo "$f"; done; true"#, "_"];
    argv.extend(candidates.iter().map(String::as_str));
    let found: Vec<String> = match crate::sandbox::exec_with(client, pod_name, "server", &argv, None).await {
        Ok(result) => result.stdout.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    };
    session::project_root(path, markers, |dir| markers.iter().any(|m| found.contains(&format!("{dir}/{m}"))))
}
