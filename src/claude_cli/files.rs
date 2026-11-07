//! What a conversation leaves in the CLI's config dir (PR #285 review 1
//! L20): its working directory, `work/<id>`, and its sessions, under
//! `projects/<the work dir's slug>/`. The sessions hold its tool output
//! and the account email the CLI adds, so they go with the conversation,
//! and a replaced session goes at once. Best effort, and logged.

use std::path::{Path, PathBuf};

/// The CLI's config dir, as a turn's process gets it.
fn config_dir() -> Option<PathBuf> {
    let env = super::cli_env(|name| std::env::var_os(name));
    env.into_iter().find(|(name, _)| name == "CLAUDE_CONFIG_DIR").map(|(_, dir)| PathBuf::from(dir))
}

/// A conversation's working directory.
pub fn work_dir(config_dir: &Path, conversation_id: i64) -> PathBuf {
    config_dir.join("work").join(conversation_id.to_string())
}

/// Where the CLI keeps the sessions of one working directory: its path
/// with every character but a letter or digit made `-` (as the capture's
/// `memory_paths` shows).
pub fn sessions_dir(config_dir: &Path, work_dir: &Path) -> PathBuf {
    let slug: String = work_dir.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    config_dir.join("projects").join(slug)
}

fn remove(path: &Path, conversation_id: i64) {
    let removed = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    match removed {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(conversation_id, path = %path.display(), "couldn't remove a Claude Code file: {e}"),
    }
}

/// Removes everything the CLI kept for `conversation_id`, on a blocking
/// thread: a directory tree's removal is blocking file work, and this
/// runs on the delete's async path (SME-430's review note, SME-431).
pub async fn forget_conversation(conversation_id: i64) {
    let Some(config_dir) = config_dir() else {
        return;
    };
    let removed = tokio::task::spawn_blocking(move || {
        let work = work_dir(&config_dir, conversation_id);
        remove(&sessions_dir(&config_dir, &work), conversation_id);
        remove(&work, conversation_id);
    })
    .await;
    if let Err(e) = removed {
        tracing::warn!(conversation_id, "couldn't remove the conversation's Claude Code files: {e}");
    }
}

/// Removes session `session_id` of `conversation_id`, once replaced.
pub fn forget_session(conversation_id: i64, session_id: &str) {
    let Some(config_dir) = config_dir() else {
        return;
    };
    // Only an id as the CLI makes them: never a path.
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return;
    }
    let sessions = sessions_dir(&config_dir, &work_dir(&config_dir, conversation_id));
    remove(&sessions.join(format!("{session_id}.jsonl")), conversation_id);
    remove(&sessions.join(session_id), conversation_id);
}
