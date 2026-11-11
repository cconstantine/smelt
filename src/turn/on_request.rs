//! Compacting on demand (SME-482): the model's `compact_context` call,
//! compacted at the end of its tool pass, at most once a turn.
//!
//! The request lives in the conversation's `CompactionProgress`
//! (`OnRequest`): the tool's `run` records it (`request`), the turn loop's
//! end of pass takes it (`take_request`, which spends the turn's once), and
//! the loop's start clears both (`turn_started`). All three run under the
//! turn lock. A window change (`sync`) keeps it: a window learned mid-turn
//! must not hand the turn a second compaction.

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
/// results, so the boundary is always safe. A failed summary fails the
/// turn as a proactive compaction's does, with the history as it was.
pub(super) async fn compact_if_asked(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    last_known_usage: &mut Option<anthropic::TokenUsage>,
    persisted: &mut Vec<Message>,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
) -> ServerFnResult<()> {
    let Some(note) = with_compaction_progress(conversation_id, CompactionProgress::take_request) else {
        return Ok(());
    };
    match compact_on_request(pool, conversation_id, turn_model, slot, Asker::Model, note).await? {
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
            .unwrap_or_else(|| Err(refused(TURN_RUNNING)))
    }));
    work.await.map_err(ServerFnError::new)?
}

const TURN_RUNNING: &str =
    "A turn is running in this conversation: compact it once the turn ends, or ask the model to call compact_context.";

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
