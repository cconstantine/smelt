//! Turn state written from outside a turn, for a conversation that may
//! already be deleted: the user's Stop, and an error shown for something
//! that failed outside a turn (SME-219).
//!
//! Each writes first, then checks the conversation still exists, and
//! forgets its turn state if it's gone, as the delete does. The delete
//! forgets only after its commit, so a check that still sees the
//! conversation came before that forget, which then clears what was
//! written; checked before the write instead, a delete could commit and
//! forget between the check and the write, and the write would stay for
//! the life of the process.

use super::*;

/// The user's Stop (`stop_turn_now`), for a conversation that may already
/// be deleted: one that is gone leaves no turn state behind (SME-219).
/// Server code stopping another conversation (one it hasn't just checked)
/// calls this too.
pub(crate) async fn stop_conversation(pool: &PgPool, conversation_id: i64) {
    stop_turn_now(conversation_id);
    forget_turn_state_if_deleted(pool, conversation_id).await;
}

/// `show_conversation_error`, for a conversation that may already be
/// deleted, likewise.
pub(crate) async fn show_error_outside_turn(pool: &PgPool, conversation_id: i64, message: String) {
    show_conversation_error(conversation_id, message);
    forget_turn_state_if_deleted(pool, conversation_id).await;
}

/// After a write to the turn state: if the conversation is gone, forgets
/// its state as the delete does. Checked after the write, never before
/// (see the module doc). A failed check is logged and the state kept.
async fn forget_turn_state_if_deleted(pool: &PgPool, conversation_id: i64) {
    match db::conversation_exists(pool, conversation_id).await {
        Ok(true) => {}
        Ok(false) => forget_conversation_lock(conversation_id),
        Err(e) => tracing::warn!(
            conversation_id,
            error = %e,
            "couldn't check a conversation still exists after a stop or an error"
        ),
    }
}
