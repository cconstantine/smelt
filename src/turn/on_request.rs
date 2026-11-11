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
