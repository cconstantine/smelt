//! Notices saved between turns, and waking the model for them.

use super::*;

/// Saves `text` as a `user` notice in `conversation_id` once no turn is
/// running there, by taking the conversation's turn lock first — so the
/// notice can never land between a tool call and its result, an order the
/// API rejects. For things that happen outside a turn and that the model
/// should learn about: its pod crashing, or the user stopping it. Waits
/// for a running turn to end, so call it from a spawned task.
#[cfg(feature = "server")]
pub(super) async fn save_notice_between_turns(
    pool: &PgPool,
    conversation_id: i64,
    text: String,
) -> Result<Message, sqlx::Error> {
    let saved = {
        let lock = conversation_lock(conversation_id);
        let _turn = lock.lock().await;
        db::create_message(pool, conversation_id, "user", &[anthropic::ContentBlock::Text { text }]).await
    };
    // Unless a turn is in flight, whose end does it.
    if !turn_running(conversation_id) {
        release_idle_turn_state(conversation_id);
    }
    let saved = saved?;
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::MessagesAppended {
            messages: vec![saved.clone()],
        },
    );
    Ok(saved)
}

/// Tells the model about something that happened outside a turn (Docker
/// restarting, the user's trust decision) and lets it answer: the notice
/// is the next message, and a turn runs for it once no turn is running.
/// After the user stopped the conversation, it's only saved, for their
/// next message. Call it from a spawned task: it waits for a running turn.
#[cfg(feature = "server")]
pub(super) async fn deliver_notice(pool: &PgPool, conversation_id: i64, text: String) {
    if is_paused(conversation_id) {
        if let Err(e) = save_notice_between_turns(pool, conversation_id, text).await {
            tracing::warn!(conversation_id, error = %e, "couldn't save a notice");
        }
        return;
    }
    // The notice as the turn's own message: `run_turn` saves it before the
    // model call, so it survives the call failing.
    let message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text }],
    };
    if let Err(TurnFailure::Failed(e)) = run_turn(pool, conversation_id, message).await {
        tracing::warn!(conversation_id, error = %e, "a notice didn't reach the model");
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::NotificationDeliveryFailed {
                detail: chat_error_text(&e),
            },
        );
    }
}

/// Wakes `conversation_id`'s turn loop because a terminal command reached a
/// terminal state (finished or lost) — no synthetic message of its own,
/// unlike `run_turn`; just triggers the same backlog drain
/// `run_turn_bounded`'s loop already does on every iteration. A true no-op
/// if, by the time this acquires the conversation lock, nothing is actually
/// pending (e.g. another concurrent wake, or an unrelated live message,
/// already handled it) — no persisted message, no API call. This is what
/// keeps several commands finishing close together from costing one model
/// turn each. See SME-13.
///
/// On failure, publishes `ConversationEvent::NotificationDeliveryFailed`
/// (alongside a `tracing::warn!`) so a watching browser tab sees it live —
/// there's usually no `send_message` call in flight to relay a `ChatEvent`
/// through, since the whole point of this function is firing with no
/// request active. The underlying notification text is unaffected either
/// way: it's already durably persisted by the drain step, which runs and
/// commits *before* the API call that might fail, so this only means the
/// model hasn't been prompted with it yet, not that it's lost.
#[cfg(feature = "server")]
pub(super) async fn wake_conversation(
    pool: &PgPool,
    conversation_id: i64,
) -> TurnResult {
    // After the user stopped this conversation, pending notices wait for
    // their next message, which drains them the same way (see `ConversationRuntime::paused`).
    if is_paused(conversation_id) {
        return Ok(Vec::new());
    }
    let result = run_turn_bounded(pool, conversation_id, None, MAX_TURNS, false).await;
    // A stop is the user's doing, not a failure to reach the model.
    if let Err(TurnFailure::Failed(e)) = &result {
        tracing::warn!(conversation_id, error = %e, "wake_conversation failed to notify the model");
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::NotificationDeliveryFailed {
                detail: chat_error_text(e),
            },
        );
    }
    result
}

/// The real body of `run_turn`, with the turn-loop bound as a parameter —
/// exists only so `test_run_turn_errors_when_max_turns_exceeded` can prove
/// the "give up and error" behavior without actually replaying the mock
/// upstream `MAX_TURNS` (10,000) times. Every other caller goes through
/// `run_turn`, which always passes the real `MAX_TURNS`.
/// Drains any terminal commands finished (or lost) but not yet notified
/// into `persisted` as ordinary persisted `user` messages, folding their
/// content into `pending_new_content` too (the compaction trigger's
/// "what's new since the last real response" estimate — see
/// `run_turn_bounded`'s own use of it) — pulled out of `run_turn_bounded`'s
/// loop so `wake_conversation` (which has no synthetic message of its own
/// to send) can trigger exactly this step directly, without duplicating
/// the notification-text logic.
#[cfg(feature = "server")]
pub(super) async fn drain_unnotified_terminal_commands(
    pool: &PgPool,
    conversation_id: i64,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<()> {
    for command in db::unnotified_finished_terminal_commands(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?
    {
        let text = if command.status == "finished" {
            format!(
                "Terminal command {} finished: exit code {}.",
                command.command_id,
                command
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        } else {
            format!(
                "Terminal command {}'s outcome is unknown — the terminal became \
                 unreachable while it was running.",
                command.command_id
            )
        };
        let notification_content = vec![anthropic::ContentBlock::Text { text }];
        // The notice and the command's mark together, so a Stop between
        // them can't leave a notice the next drain saves again (SME-91).
        let Some(saved) =
            db::save_command_notice(pool, conversation_id, &command.command_id, &notification_content)
                .await
                .map_err(ServerFnError::new)?
        else {
            continue;
        };
        pending_new_content.extend(notification_content);
        record_saved(conversation_id, persisted, saved);
    }
    Ok(())
}

/// What `notify` does with one notice.
#[derive(Debug)]
pub(crate) enum Notice {
    /// Saved between turns, without waking the model: it reads it on its
    /// next turn (the user stopping a pod).
    Save(String),
    /// Saved and answered: a turn runs for it, unless the user has stopped
    /// the conversation, when it's only saved (Docker restarting, the
    /// user's trust decision).
    Deliver(String),
    /// Saves finished commands' notices and wakes the model for them; a
    /// no-op when none is pending or the conversation is stopped.
    Wake,
}

/// Tells `conversation_id`'s model about things that happened outside a
/// turn, in order, in a task of its own: each notice waits for a running
/// turn, and the turn lock isn't re-entrant, so a caller inside a turn
/// (a tool reaching a crashed pod, say) can't wait for it. Returns at
/// once. Failures are logged; a delivery or wake that fails is also
/// published (`NotificationDeliveryFailed`). The one way anything outside
/// a turn reaches the model (SME-52).
pub(crate) fn notify(pool: &PgPool, conversation_id: i64, notices: Vec<Notice>) -> tokio::task::JoinHandle<()> {
    let pool = pool.clone();
    tokio::spawn(async move {
        for notice in notices {
            match notice {
                Notice::Save(text) => {
                    if let Err(e) = save_notice_between_turns(&pool, conversation_id, text).await {
                        tracing::warn!(conversation_id, error = %e, "couldn't save a notice");
                    }
                }
                Notice::Deliver(text) => deliver_notice(&pool, conversation_id, text).await,
                Notice::Wake => {
                    // A failure is already reported inside.
                    let _ = wake_conversation(&pool, conversation_id).await;
                }
            }
        }
    })
}
