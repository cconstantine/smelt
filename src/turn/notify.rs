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
    // A saved notice re-sorts the sidebar's row like any other message (SME-269).
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    Ok(saved)
}

/// Tells the model about something that happened outside a turn (Docker
/// restarting, the user's trust decision) and lets it answer: the notice
/// is queued (`db::queue_server_notice`) and a wake drains it, at a
/// running turn's next step or in a turn of its own (D5), so a restart
/// before it lands loses nothing: the startup sweep wakes what's queued
/// (SME-450). After the user stopped the conversation, or to a child
/// that takes no notices, it's only saved, at once, for the next turn
/// (D4). Call it from a spawned task: it waits for a running turn.
#[cfg(feature = "server")]
pub(super) async fn deliver_notice(pool: &PgPool, conversation_id: i64, text: String) {
    // A child still starting, stopped or failed is only told (SME-236):
    // its next turn, if it has one, reads it.
    if !crate::agents::admits_notices(pool, conversation_id).await || is_paused(conversation_id) {
        if let Err(e) = save_notice_between_turns(pool, conversation_id, text).await {
            tracing::warn!(conversation_id, error = %e, "couldn't save a notice");
        }
        return;
    }
    match db::queue_server_notice(pool, conversation_id, &text).await {
        Ok(()) => {
            // A failure is already reported inside.
            let woke = wake_conversation(pool, conversation_id).await;
            // A Stop that ended (or refused) the wake leaves the notice
            // queued: shown at once all the same, as before the queue and
            // as a paused conversation's is (D9).
            if matches!(woke, Err(TurnFailure::Stopped)) || is_paused(conversation_id) {
                save_queued_server_notices(pool, conversation_id).await;
            }
        }
        // Today's way, in memory: a database error costs durability only.
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't queue a notice; delivering it directly");
            deliver_notice_directly(pool, conversation_id, text).await;
        }
    }
}

/// Saves `conversation_id`'s queued `server_notice` rows into its
/// transcript, holding the turn lock, with no model call (D9): each
/// through `db::deliver_agent_message`, so a drain racing it can't save
/// one twice. Other kinds stay queued for the next turn.
#[cfg(feature = "server")]
async fn save_queued_server_notices(pool: &PgPool, conversation_id: i64) {
    let saved = {
        let lock = conversation_lock(conversation_id);
        let _turn = lock.lock().await;
        let mut saved = Vec::new();
        match db::undelivered_agent_messages(pool, conversation_id).await {
            Ok(rows) => {
                for row in rows.iter().filter(|row| row.kind == db::KIND_SERVER_NOTICE) {
                    let content = [anthropic::ContentBlock::Text { text: agent_message_text(row) }];
                    match db::deliver_agent_message(pool, row, &content).await {
                        Ok(Some(message)) => saved.push(message),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't save a queued notice"),
                    }
                }
            }
            Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't read the queued notices"),
        }
        saved
    };
    if !turn_running(conversation_id) {
        release_idle_turn_state(conversation_id);
    }
    if saved.is_empty() {
        return;
    }
    crate::events::publish(conversation_id, crate::events::ConversationEvent::MessagesAppended { messages: saved });
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
}

/// `deliver_notice` without the queue: the notice is the next turn's own
/// message, held in this task until the turn lock is free.
#[cfg(feature = "server")]
async fn deliver_notice_directly(pool: &PgPool, conversation_id: i64, text: String) {
    // Let in past the pause in the same step that counts it, so a Stop
    // either refuses it or ends it (SME-217).
    let Some(turn) = TurnInFlight::start_unless_paused(conversation_id) else {
        if let Err(e) = save_notice_between_turns(pool, conversation_id, text).await {
            tracing::warn!(conversation_id, error = %e, "couldn't save a notice");
        }
        return;
    };
    #[cfg(test)]
    test_hooks::after_notice_admitted(conversation_id).await;
    // The notice as the turn's own message: the turn saves it before the
    // model call, so it survives the call failing.
    let message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text }],
    };
    if let Err(TurnFailure::Failed(e)) = run_turn_bounded(pool, turn, Some(message), MAX_TURNS, None, false).await {
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
/// unlike a send's or a notice's turn; just triggers the same backlog drain
/// `run_turn_bounded`'s loop already does on every iteration. A true no-op
/// if, by the time this acquires the conversation lock, nothing is actually
/// pending (e.g. another concurrent wake, or an unrelated live message,
/// already handled it) — no persisted message, no API call. This is what
/// keeps several commands finishing close together from costing one model
/// turn each. See SME-13. One that finds nothing pending before it's counted
/// isn't counted at all, so no tab is told a turn started: it would show
/// "Working…" and clear the error it shows (SME-218).
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
    // Anything to do is asked first, before the step below that counts
    // the wake (which can't wait on the database): a wake with nothing
    // isn't counted, so no tab is told a turn started (SME-218). The drain
    // under the turn lock still decides; this only skips wakes sure to
    // find nothing.
    match db::wake_has_work(pool, conversation_id).await {
        // An answer owed with no turn running is one a restart dropped
        // (SME-450): this wake answers it. While a turn runs, the mark is
        // that turn's own, so a wake with nothing else still isn't
        // counted (SME-218).
        Ok(false) if turn_running(conversation_id) => return Ok(Vec::new()),
        Ok(false) => match db::answer_owed(pool, conversation_id).await {
            Ok(true) => {}
            Ok(false) => return Ok(Vec::new()),
            Err(e) => {
                tracing::warn!(conversation_id, error = %e, "couldn't check a wake for an answer owed");
                return Ok(Vec::new());
            }
        },
        Ok(true) => {}
        // Let in as before, so a notice is never dropped; the drain
        // reports the failure if it persists.
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't check a wake for pending work"),
    }
    // A child still starting, stopped or failed takes no turn (SME-236):
    // the start wakes one once it's active, the user's send a stopped one.
    if !crate::agents::admits_notices(pool, conversation_id).await {
        return Ok(Vec::new());
    }
    // After the user stopped this conversation, pending notices wait for
    // their next message, which drains them the same way (see `ConversationRuntime::paused`).
    // Checked in the same step that counts the wake, so a Stop either
    // refuses it or ends it (SME-217).
    let Some(turn) = TurnInFlight::start_unless_paused(conversation_id) else {
        return Ok(Vec::new());
    };
    #[cfg(test)]
    test_hooks::after_notice_admitted(conversation_id).await;
    let result = run_turn_bounded(pool, turn, None, MAX_TURNS, None, false).await;
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
            crate::api::chat::mark_notice(&format!(
                "Terminal command {} finished: exit code {}.",
                command.command_id,
                command
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ))
        } else {
            crate::api::chat::mark_notice(&format!(
                "Terminal command {}'s outcome is unknown — the terminal became \
                 unreachable while it was running.",
                command.command_id
            ))
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
        // A command's end is news for the model, so the turn is pod
        // activity (SME-78).
        set_activity_owed(conversation_id);
    }
    Ok(())
}

/// The idle notice's own fixed words, written only by the one `format!` in
/// `drain_idle_command_notices`. Nothing on the server reads it since
/// SME-377 removed the automatic Finished that `saved_idle_notice` (SME-370)
/// used it to quiet; the transcript's short line still splits the stored
/// sentence on a copy of it. Re-wording the notice must keep the two matching: wake's
/// `test_wake_drains_an_idle_notice_and_the_command_keeps_running` pins
/// the sentence in parts and pins this clause as the test's own literal —
/// never through this const, which could never go red for guarding it —
/// and its failure message names this const and the transcript copy
/// (`pages/chat/transcript/text.rs`) to re-word together (review 1).
#[cfg(feature = "server")]
const IDLE_NOTICE_FRAGMENT: &str = "is still running and hasn't been touched in";

/// The second half of the drain, run with
/// `drain_unnotified_terminal_commands` in the same place: commands still
/// *running* whose idle notice is due (SME-79) — untouched for their whole
/// interval. smelt never stops them; this tells the model one is still
/// going, with the numbers it needs to decide, and the model decides. Saved
/// through `db::save_idle_notice`, in the conversation's locked step like
/// every other notice, and only while the command is *still* due — one that
/// finished or was touched since the watch woke this conversation gets
/// nothing. Each one restarts the command's clock, so a command nobody
/// touches is told about again, every interval, without limit.
#[cfg(feature = "server")]
pub(super) async fn drain_idle_command_notices(
    pool: &PgPool,
    conversation_id: i64,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<()> {
    for command in db::idle_commands_due(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?
    {
        let text = crate::api::chat::mark_notice(&format!(
            "Terminal command {id} (\"{shown}\", running for {running}) {IDLE_NOTICE_FRAGMENT} \
             {untouched}: {stdout} lines on stdout, {stderr} on stderr. \
             Nothing stopped it and smelt won't stop it. Read what it has printed with \
             read_terminal_output, check it with terminal_command_status, interrupt it with \
             send_signal (INT, then KILL if it ignores that), or run your next command in \
             another terminal. To stop these notices for it, call set_idle_notice with \
             idle_notice_seconds 0 or a longer interval.",
            id = command.command_id,
            shown = crate::anthropic::tools::shown_command(&command.command),
            running = format_duration(command.running_for_secs),
            untouched = format_duration(command.untouched_for_secs),
            stdout = command.stdout_lines,
            stderr = command.stderr_lines,
        ));
        let notice_content = vec![anthropic::ContentBlock::Text { text }];
        // The notice and the mark together, so a Stop between them can't
        // leave a notice the next drain saves again (SME-91).
        let Some(saved) =
            db::save_idle_notice(pool, conversation_id, &command.command_id, &notice_content)
                .await
                .map_err(ServerFnError::new)?
        else {
            continue;
        };
        pending_new_content.extend(notice_content);
        record_saved(conversation_id, persisted, saved);
    }
    Ok(())
}

/// Drains the reports and notices queued to `conversation_id`
/// (`agent_messages`, SME-236), in the order they were queued: each saved
/// as one `user` message naming its sender and marked delivered, in one
/// transaction per row, so a stop between rows leaves the rest queued and
/// none saved twice. Run with the command drains, at every iteration, so
/// a busy recipient takes them between a tool result and its next call.
#[cfg(feature = "server")]
pub(super) async fn drain_agent_messages(
    pool: &PgPool,
    conversation_id: i64,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
    persisted: &mut Vec<Message>,
) -> ServerFnResult<()> {
    for row in db::undelivered_agent_messages(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?
    {
        let content = vec![anthropic::ContentBlock::Text { text: agent_message_text(&row) }];
        let Some(saved) = db::deliver_agent_message(pool, &row, &content)
            .await
            .map_err(ServerFnError::new)?
        else {
            continue;
        };
        pending_new_content.extend(content);
        record_saved(conversation_id, persisted, saved);
        // An agent's report or notice is news for the model (SME-78).
        set_activity_owed(conversation_id);
    }
    Ok(())
}

/// A queued row as its recipient reads it: a report, a message or a
/// question names its sender (`agent #12 (smelt:developer)`, or `your
/// parent`), and a question says how to answer it.
fn agent_message_text(row: &db::AgentMessage) -> String {
    match row.kind.as_str() {
        db::KIND_REPORT => format!("Report from {}:\n\n{}", row.from_label, row.body),
        db::KIND_MESSAGE => format!("Message from {}:\n\n{}", row.from_label, row.body),
        db::KIND_QUESTION => match row.from_conversation_id {
            Some(from) => format!(
                "Question from {}, which waits for your answer (reply with message_agent to {from}):\n\n{}",
                row.from_label, row.body
            ),
            None => format!("Question from {} (since deleted):\n\n{}", row.from_label, row.body),
        },
        // A `Notice::Deliver`'s words, already marked, as said (SME-450).
        db::KIND_SERVER_NOTICE => row.body.clone(),
        // SME-357: a queued notice is server-written, so it carries the mark
        // like every saved notice; reports, messages and questions are
        // another agent's typed words and stay unmarked.
        _ => crate::api::chat::mark_notice(&format!("Notice: {}", row.body)),
    }
}

/// A duration in seconds, said in words: "45 seconds", "5 minutes",
/// "2 hours 5 minutes", "3 days 4 hours" (SME-79). The two biggest units that
/// fit, rounded down — a notice never overstates how long a command has been
/// left alone. Shared by the idle notice and `set_idle_notice`'s reply.
pub(crate) fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        count(secs, "second")
    } else if secs < 3_600 {
        spoken(secs / 60, secs % 60, "minute", "second")
    } else if secs < 86_400 {
        spoken(secs / 3_600, secs % 3_600 / 60, "hour", "minute")
    } else {
        spoken(secs / 86_400, secs % 86_400 / 3_600, "day", "hour")
    }
}

/// The two units that fit a duration's sentence, the smaller one only when
/// there's something to say: "5 minutes", "2 hours 5 minutes" (SME-79).
fn spoken(big: i64, small: i64, big_unit: &str, small_unit: &str) -> String {
    if small == 0 {
        count(big, big_unit)
    } else {
        format!("{} {}", count(big, big_unit), count(small, small_unit))
    }
}

/// A count and its unit, pluralised the way the sentence reads: "1 minute",
/// "5 minutes" (SME-79).
fn count(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// What `notify` does with one notice.
#[derive(Debug)]
pub(crate) enum Notice {
    /// Saved between turns, without waking the model: it reads it on its
    /// next turn (the user stopping a pod).
    Save(String),
    /// Queued and answered: a wake drains it (a running turn's next step
    /// takes it first) and the model answers, and a restart before then
    /// loses nothing (SME-450). Unless the user has stopped the
    /// conversation, when it's only saved (Docker restarting, the user's
    /// trust decision).
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
