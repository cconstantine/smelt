//! Compacting on demand (SME-482): the model's `compact_context` call,
//! compacted at the end of its tool pass, at most once a turn.
//!
//! The request lives in the conversation's `CompactionProgress`
//! (`OnRequest`): the tool's `run` records it (`request`), the turn loop's
//! end of pass takes it (`take_request`, which spends the turn's once), and
//! the loop's start clears both (`turn_started`). All three run under the
//! turn lock. A window change (`sync`) keeps it: a window learned mid-turn
//! must not hand the turn a second compaction.
//!
//! A persona's token ceiling (SME-493, `compact_at_ceiling`) compacts the
//! same way, before a model call, once the conversation passes it.

use super::*;

/// The model's `compact_context`: records the request for the end of this
/// tool pass, or says why it can't be (already asked in this reply, or
/// already compacted once this turn).
pub(crate) fn request_compaction(conversation_id: i64, note: Option<String>) -> Result<(), String> {
    with_compaction_progress(conversation_id, |p| p.request(note)).map_err(str::to_string)
}

/// At the end of a tool pass, after every result is saved and before the
/// next model call (or the turn's return): compacts when the pass's
/// `compact_context` asked for it. The last saved message is the pass's
/// results, so the boundary is always safe. A failed summary doesn't fail
/// the turn (round 1 L1): this compaction is the model's choice, not one the
/// next request needs, so the model is told it failed, the history stays as
/// it was (SME-443), and the turn carries on.
/// The tool pass may have given the turn's slot to its own live agent
/// (SME-354 B6), so it's taken back first: the summary call never goes
/// out beside the child's on a limited provider (round 1 M1).
pub(super) async fn compact_if_asked(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    slot: &mut TurnSlot,
    last_known_usage: &mut Option<anthropic::TokenUsage>,
    persisted: &mut Vec<Message>,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
) -> ServerFnResult<()> {
    let Some(note) = with_compaction_progress(conversation_id, CompactionProgress::take_request) else {
        return Ok(());
    };
    slot.refresh(turn_model).await;
    let compacted = match compact_on_request(pool, conversation_id, turn_model, slot.held(), Asker::Model, note).await {
        Ok(compacted) => compacted,
        Err(e) => {
            let why = crate::api::chat::chat_error_text(&e);
            tracing::warn!(conversation_id, error = %why, "the model's compaction failed; the turn carries on");
            let notice = format!(
                "smelt notice: the compaction compact_context asked for failed, so the history is unchanged: {why} \
                 Carry on from here."
            );
            save_notice(pool, conversation_id, crate::api::chat::mark_notice(&notice), persisted, pending_new_content).await?;
            return Ok(());
        }
    };
    match compacted {
        Compacted::Done => {
            let window = turn_model.context_window;
            with_compaction_progress(conversation_id, |p| p.compacted(window));
            // The continuation is now the request's tail, estimated whole
            // as after any compaction.
            *last_known_usage = None;
            pending_new_content.clear();
            note_thrash(pool, conversation_id, persisted, pending_new_content).await?;
        }
        // Can't happen after a saved pass, which ends on its results.
        Compacted::Skipped(why) => tracing::warn!(conversation_id, ?why, "the model's compaction found nothing to compact"),
    }
    Ok(())
}

/// Before a model call the window's trigger let go (SME-493): compacts
/// when the request, about `projected` tokens, is at or over the persona's
/// ceiling and `ceiling_due` says it's worth it, over the same
/// `compact_on_request` path, so every safeguard of `compact` holds.
/// Returns whether it compacted. A failed summary doesn't fail the turn:
/// the request still fits the window, so the ceiling is a saving, not a
/// need. Nothing is saved for the model (it didn't ask, and nothing
/// changed for it); the ceiling is off for the rest of the turn.
pub(super) async fn compact_at_ceiling(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    slot: &TurnSlot,
    projected: Option<u64>,
    persisted: &mut Vec<Message>,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
) -> ServerFnResult<bool> {
    let Some(at) = with_compaction_progress(conversation_id, |p| p.ceiling_due(projected)) else {
        return Ok(false);
    };
    match compact_on_request(pool, conversation_id, turn_model, slot.held(), Asker::Ceiling(at), None).await {
        Ok(Compacted::Done) => {
            let window = turn_model.context_window;
            with_compaction_progress(conversation_id, |p| p.compacted(window));
            note_thrash(pool, conversation_id, persisted, pending_new_content).await?;
            Ok(true)
        }
        // Not expected at this site, which the window's own compaction
        // shares; off for the turn like a failure, so it's warned of once a
        // turn rather than at every step (round 1 L4).
        Ok(Compacted::Skipped(why)) => {
            tracing::warn!(conversation_id, ?why, "the ceiling's compaction found nothing to compact");
            with_compaction_progress(conversation_id, CompactionProgress::ceiling_failed);
            Ok(false)
        }
        Err(e) => {
            let why = crate::api::chat::chat_error_text(&e);
            tracing::warn!(conversation_id, error = %why, ceiling = at, "the ceiling's compaction failed; the turn carries on");
            with_compaction_progress(conversation_id, CompactionProgress::ceiling_failed);
            Ok(false)
        }
    }
}

/// A request the endpoint refuses, in its own words: a 409, since the
/// conversation's state is what's in the way, never the request.
fn refused(message: &str) -> ServerFnError {
    ServerFnError::ServerError { message: message.to_string(), code: 409, details: None }
}

/// The endpoint's compaction (SME-482): compacts `conversation_id` now,
/// between turns, with `note` for the summariser. Refused at once, never
/// queued, while a turn runs or is queued; refused too while a question
/// waits (covering its call would orphan the answer), when the last
/// reply's calls have no results, and when nothing is new since the last
/// compaction. The work runs in a task of its own, which this awaits: a
/// client hanging up drops only the wait, never a compaction halfway
/// through its saves (the `after_turn_state_write` pattern, SME-219).
pub(crate) async fn compact_now(pool: &PgPool, conversation_id: i64, note: Option<String>) -> ServerFnResult<()> {
    let note = anthropic::tools::parse_note(note.as_deref()).map_err(|e| refused(&e))?;
    if !db::conversation_exists(pool, conversation_id).await.map_err(ServerFnError::new)? {
        return Err(crate::api::chat::conversation_not_found());
    }
    let pool = pool.clone();
    let work = tokio::spawn(tracing::Instrument::in_current_span(async move {
        if turn_running(conversation_id) {
            return Err(refused(TURN_RUNNING));
        }
        between_turns(conversation_id, false, || compact_between_turns(&pool, conversation_id, note))
            .await
            // The lock is held, but by no turn (another compaction, an idle
            // pass): not "a turn is running" (round 1 L4).
            .unwrap_or_else(|| Err(refused(if turn_running(conversation_id) { TURN_RUNNING } else { BUSY })))
    }));
    work.await.map_err(ServerFnError::new)?
}

const TURN_RUNNING: &str =
    "A turn is running in this conversation: compact it once the turn ends, or ask the model to call compact_context.";

const BUSY: &str = "This conversation is busy (another compaction, or smelt's own work between turns): try again in a moment.";

/// `compact_now`'s work, holding the turn lock.
async fn compact_between_turns(pool: &PgPool, conversation_id: i64, note: Option<String>) -> ServerFnResult<()> {
    if db::get_pending_question(pool, conversation_id).await.map_err(ServerFnError::new)?.is_some() {
        return Err(refused("This conversation is waiting on a question: answer it first."));
    }
    let messages = db::list_messages(pool, conversation_id).await.map_err(ServerFnError::new)?;
    if nothing_since_the_last_compaction(&messages) {
        return Err(refused("Nothing new since the last compaction."));
    }
    let mut turn_model = crate::providers::resolve_turn_model(pool, conversation_id).await.map_err(ServerFnError::new)?;
    let slot = TurnSlot::acquire(&turn_model).await;
    match compact_on_request(pool, conversation_id, &mut turn_model, slot.held(), Asker::Request, note).await? {
        Compacted::Done => {
            let window = turn_model.context_window;
            with_compaction_progress(conversation_id, |p| p.compacted(window));
            Ok(())
        }
        Compacted::Skipped(Skip::Empty) => Err(refused("Nothing to compact yet.")),
        Compacted::Skipped(Skip::UnsafeBoundary) => Err(refused(
            "The last reply's tool calls have no results yet: send a message first, then compact.",
        )),
    }
}

/// Whether everything after the latest compaction summary is only that
/// compaction's own continuation: a compaction now would summarise a
/// summary. `false` with no compaction yet.
fn nothing_since_the_last_compaction(messages: &[Message]) -> bool {
    let blocks = |m: &Message| m.blocks().unwrap_or_default();
    let Some(summary) = messages
        .iter()
        .rposition(|m| blocks(m).iter().any(|b| matches!(b, anthropic::ContentBlock::CompactionSummary { .. })))
    else {
        return false;
    };
    messages[summary + 1..]
        .iter()
        .all(|m| blocks(m).iter().all(|b| matches!(b, anthropic::ContentBlock::CompactionPlaceholder { .. })))
}
