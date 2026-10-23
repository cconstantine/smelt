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
/// The pause is stored (SME-238) in the check's own task, so a caller
/// cancelled after the write still stores it and still checks.
pub(crate) async fn stop_conversation(pool: &PgPool, conversation_id: i64) {
    stop_turn_now(conversation_id);
    after_turn_state_write(pool, conversation_id, true).await;
}

/// Writes `conversation_id`'s in-memory pause to `paused_at`, so it
/// survives a restart (SME-238). Memory stays the live truth: the pause is
/// decided in a turn's admission, which a database read can't join. So
/// this reads memory, writes it, and reads again, writing again while it
/// changed: a Stop and a send racing each end with a write of the last
/// value. A failed write is logged; memory is still right for this run.
pub(crate) async fn store_pause(pool: &PgPool, conversation_id: i64) {
    #[cfg(test)]
    let _done = crate::agents::test_hooks::StoreDone(conversation_id);
    let mut paused = is_paused(conversation_id);
    for _ in 0..8 {
        #[cfg(test)]
        crate::agents::test_hooks::reach(crate::agents::test_hooks::STORE_PAUSE_READ, conversation_id).await;
        if let Err(e) = db::set_paused(pool, conversation_id, paused).await {
            tracing::warn!(conversation_id, error = %e, "couldn't store a conversation's pause");
            return;
        }
        let now = is_paused(conversation_id);
        if now == paused {
            return;
        }
        paused = now;
    }
    tracing::warn!(conversation_id, "a conversation's pause kept changing while it was stored; the stored one may be stale");
}

/// `store_pause` in a task of its own, for the user's send or answer
/// right after its admission ended the pause: a request cancelled after
/// the admission can't stop it, and the turn reaches its place in the
/// queue with no write in front of it (a Stop sent just after must find
/// it waiting there, SME-216).
pub(crate) fn spawn_store_pause(pool: &PgPool, conversation_id: i64) {
    let pool = pool.clone();
    tokio::spawn(tracing::Instrument::in_current_span(async move {
        store_pause(&pool, conversation_id).await;
    }));
}

/// At startup, before anything can wake a conversation: pauses every
/// conversation whose pause is stored (SME-238). A failed read is logged;
/// the startup sweep's own queries still skip stored pauses.
pub(crate) async fn restore_pauses(pool: &PgPool) {
    match db::paused_conversations(pool).await {
        Ok(ids) => {
            for id in ids {
                restore_pause(id);
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't read the stored pauses; paused conversations may be woken"),
    }
}

/// `show_conversation_error`, for a conversation that may already be
/// deleted, likewise.
pub(crate) async fn show_error_outside_turn(pool: &PgPool, conversation_id: i64, message: String) {
    show_conversation_error(conversation_id, message);
    after_turn_state_write(pool, conversation_id, false).await;
}

/// After a write to the turn state: stores the pause first when
/// `store` (a Stop's), then, if the conversation is gone, forgets its
/// state as the delete does. Checked after the write, never before (see
/// the module doc). A failed check is logged and the state kept. Runs in
/// a spawned task the caller awaits, so the caller being cancelled
/// doesn't cancel it.
async fn after_turn_state_write(pool: &PgPool, conversation_id: i64, store: bool) {
    let pool = pool.clone();
    // In the caller's span, as `start_turn`'s turn is (SME-262 review 1).
    let check = tokio::spawn(tracing::Instrument::in_current_span(async move {
        if store {
            store_pause(&pool, conversation_id).await;
        }
        match db::conversation_exists(&pool, conversation_id).await {
            Ok(true) => {}
            Ok(false) => forget_conversation_lock(conversation_id),
            Err(e) => tracing::warn!(
                conversation_id,
                error = %e,
                "couldn't check a conversation still exists after a stop or an error"
            ),
        }
    }));
    if let Err(e) = check.await {
        tracing::warn!(conversation_id, error = %e, "the check after a stop or an error didn't finish");
    }
}
