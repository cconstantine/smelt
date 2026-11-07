//! A Claude Code conversation's session after smelt compacts it (SME-431,
//! SME-399's B2): the session the CLI holds is the history the summary
//! replaced, so it goes, and the next call starts a new one seeded from
//! the post-compaction request (`render::rehydrate`).
//!
//! `reset` (from `turn::compaction`, once the summary's messages are
//! saved) deletes the `claude_cli_sessions` row and marks the
//! conversation: its run is ended at its next call (`take_reset`), and
//! the replaced session's files go once its last process has ended
//! (`take_retired`, from `run::start`). A conversation that's deleted
//! drops its marks (`forget`).

use std::collections::HashMap;
use std::sync::Mutex;

/// Per conversation: whether a reset waits for its run, and the replaced
/// sessions whose files go at the next new session.
#[derive(Default)]
struct Marks {
    reset: bool,
    retired: Vec<String>,
}

fn marks() -> std::sync::MutexGuard<'static, HashMap<i64, Marks>> {
    static MARKS: std::sync::LazyLock<Mutex<HashMap<i64, Marks>>> = std::sync::LazyLock::new(Mutex::default);
    // Every change is one field set or taken: nothing is left half-done.
    MARKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends `conversation_id`'s CLI session after a compaction: a no-op for
/// any other kind of provider. Best effort, and logged: a row left
/// behind no longer matches the compacted history, so the next call
/// starts a new session anyway.
pub async fn reset(pool: &sqlx::PgPool, conversation_id: i64, kind: crate::providers::ProviderKind) {
    if kind != crate::providers::ProviderKind::ClaudeCli {
        return;
    }
    let recorded = crate::db::get_claude_cli_session(pool, conversation_id).await.unwrap_or_else(|e| {
        tracing::warn!(conversation_id, "couldn't read the conversation's Claude Code session to reset it: {e}");
        None
    });
    if let Err(e) = crate::db::delete_claude_cli_session(pool, conversation_id).await {
        tracing::warn!(conversation_id, "couldn't delete the conversation's Claude Code session after a compaction: {e}");
    }
    tracing::info!(conversation_id, "compacted: the conversation's next Claude Code call starts a new session from the summary");
    let mut marks = marks();
    let mine = marks.entry(conversation_id).or_default();
    mine.reset = true;
    if let Some(session) = recorded {
        mine.retired.push(session.session_id);
    }
}

/// Whether a reset waits for `conversation_id`'s run, cleared as it's read.
pub fn take_reset(conversation_id: i64) -> bool {
    marks().get_mut(&conversation_id).is_some_and(|m| std::mem::take(&mut m.reset))
}

/// The sessions a reset replaced, whose files can go, taken.
pub fn take_retired(conversation_id: i64) -> Vec<String> {
    marks().get_mut(&conversation_id).map(|m| std::mem::take(&mut m.retired)).unwrap_or_default()
}

/// Drops `conversation_id`'s marks: it's being deleted.
pub fn forget(conversation_id: i64) {
    marks().remove(&conversation_id);
}
