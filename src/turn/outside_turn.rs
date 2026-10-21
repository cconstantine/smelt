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
//!
//! The check runs in a task of its own, which the caller awaits: a caller
//! that is cancelled after the write (a Stop whose client hangs up, so its
//! request's future is dropped) can't stop the check, so it can't leave
//! the write behind either (SME-219 review 1). The write itself is
//! synchronous, so nothing can cut in before the task starts.

use super::*;

/// The user's Stop (`stop_turn_now`), for a conversation that may already
/// be deleted: one that is gone leaves no turn state behind (SME-219),
/// though a turn still in flight there is ended. Server code that stops
/// another conversation (SME-236's `stop_agent`) must call this too.
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
/// Runs in a spawned task the caller awaits, so the caller being
/// cancelled doesn't cancel it.
async fn forget_turn_state_if_deleted(pool: &PgPool, conversation_id: i64) {
    let pool = pool.clone();
    let check = tokio::spawn(async move {
        match db::conversation_exists(&pool, conversation_id).await {
            Ok(true) => {}
            Ok(false) => forget_conversation_lock(conversation_id),
            Err(e) => tracing::warn!(
                conversation_id,
                error = %e,
                "couldn't check a conversation still exists after a stop or an error"
            ),
        }
    });
    if let Err(e) = check.await {
        tracing::warn!(conversation_id, error = %e, "the check after a stop or an error didn't finish");
    }
}
