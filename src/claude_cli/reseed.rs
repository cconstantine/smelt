//! Re-seeding a conversation's prompt cache (SME-523): what the watch
//! (`turn::reseed`) knows of each Claude Code conversation's cache, and
//! the one-line call that refreshes it.
//!
//! **Candidates** (in memory): a turn's call passing the usage gate
//! starts one (`note_call`), from that call's start, with no re-seeds
//! yet. A re-seed that went well moves its start up to its own
//! (`note_reseed`), and never back: a real call meanwhile stands. A
//! restart loses them all, so there are no re-seeds until each
//! conversation's next call.
//!
//! **The call** (`launch`): the turn's system prompt, tools, model and
//! effort, as a wake would send them now, resuming the conversation's
//! session without saving to it (`--resume <id>
//! --no-session-persistence`), with one user line and `--max-turns 1`,
//! through `once`. Nothing of its reply is kept but its usage.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use chrono::{DateTime, Utc};

use super::once::{OnceLaunch, once};
use super::process::SessionArg;
use crate::anthropic::stream::{ModelError, StreamedTurn, UsageMeter};
use crate::anthropic::{AnthropicMessage, ContentBlock, CreateMessageRequest};

/// The re-seed's one user line.
pub const MESSAGE: &str = "Reply with only the word ok.";

/// One conversation whose cache may be kept warm.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    /// When its cache was last read: its last call's start.
    pub start: DateTime<Utc>,
    /// The provider that call went to.
    pub provider_id: i64,
    /// Re-seeds since its last real call.
    pub reseeds: u32,
    /// A re-seed of it is in flight.
    pub in_flight: bool,
}

fn candidates() -> MutexGuard<'static, HashMap<i64, Candidate>> {
    static CANDIDATES: LazyLock<Mutex<HashMap<i64, Candidate>>> = LazyLock::new(Default::default);
    // Every change is one insert, remove or field write.
    CANDIDATES.lock().unwrap_or_else(|e| e.into_inner())
}

/// A turn's call of `conversation_id` on `provider_id` passed the gate at
/// `at`: its cache is fresh from then, and its idle spell starts over.
pub fn note_call(conversation_id: i64, provider_id: i64, at: DateTime<Utc>) {
    candidates().insert(conversation_id, Candidate { start: at, provider_id, reseeds: 0, in_flight: false });
}

/// Marks `conversation_id`'s re-seed in flight; false when it's gone or
/// one already is.
pub fn begin(conversation_id: i64) -> bool {
    match candidates().get_mut(&conversation_id) {
        Some(c) if !c.in_flight => {
            c.in_flight = true;
            true
        }
        _ => false,
    }
}

/// A re-seed that started at `started` went well: the start moves up to
/// it, unless a real call has since moved it further (R4).
pub fn note_reseed(conversation_id: i64, started: DateTime<Utc>) {
    if let Some(c) = candidates().get_mut(&conversation_id) {
        c.in_flight = false;
        if c.start <= started {
            c.start = started;
            c.reseeds += 1;
        }
    }
}

/// Drops `conversation_id`: its cache is gone, or nothing will wake it.
pub fn forget(conversation_id: i64) {
    candidates().remove(&conversation_id);
}

/// `conversation_id`'s candidate, if any, for a test.
#[cfg(test)]
pub fn get(conversation_id: i64) -> Option<Candidate> {
    candidates().get(&conversation_id).copied()
}

/// Every candidate not in flight whose start is at or before `before`.
pub fn due(before: DateTime<Utc>) -> Vec<(i64, Candidate)> {
    candidates().iter().filter(|(_, c)| !c.in_flight && c.start <= before).map(|(id, c)| (*id, *c)).collect()
}

/// Clears every candidate, for a test.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    candidates().clear();
}

/// `request` (the turn's, as a wake would send it now) as the re-seed
/// sends it: the same system prompt and tools, and the one line.
pub fn reseed_request(request: &CreateMessageRequest) -> CreateMessageRequest {
    let message = AnthropicMessage { role: "user".to_string(), content: vec![ContentBlock::Text { text: MESSAGE.to_string() }] };
    CreateMessageRequest { messages: vec![message], ..request.clone() }
}

/// One re-seed of `conversation_id`'s session `session_id` with `request`
/// (`reseed_request`'s), its usage to `meter`.
pub async fn launch(
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &CreateMessageRequest,
    session_id: &str,
    meter: &UsageMeter,
) -> Result<StreamedTurn, ModelError> {
    // The turn's tools, through the bridge as a turn's run has them, so
    // the tools list (part of the cached prefix) is the wake's. Held for
    // the call: dropping it ends anything the model asked of it.
    let run_bridge = super::bridge::bridge().await.map_err(ModelError::Other)?.register(&request.tools);
    let mcp_config = (!request.tools.is_empty()).then(|| run_bridge.mcp_config());
    let launch = OnceLaunch { effort: turn_model.effort, mcp_config, session: SessionArg::ResumeUnsaved(session_id) };
    let replied = once(conversation_id, turn_model, request, launch, &mut |_| {}, meter).await;
    drop(run_bridge);
    replied
}
