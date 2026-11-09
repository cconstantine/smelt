//! The size side of compaction: the token estimate, the trigger
//! ceiling, a request projection, and reply budgets. Part of
//! `turn::compaction` (split out of its `compaction.rs`, SME-443); names
//! and visibilities unchanged, reachable through the module (the only
//! edit is pub(super)→pub on moved items, restored by mod.rs re-exports).

use super::*;

/// Headroom for the token estimate's error: kept off the context window
/// on top of the reply (`reply_budget`), and the compaction trigger's
/// floor should `MIN_REPLY_TOKENS` ever be smaller — mirrors opencode's
/// own `max(output reserve, buffer)` term. See SME-18.
#[cfg(feature = "server")]
pub const COMPACTION_SAFETY_BUFFER: u32 = 4096;

/// A cheap, approximate token-count estimate for content not yet sent —
/// chars/4, Anthropic's own rough published guidance, not a real
/// tokenizer. Used only to decide whether compaction should run before a
/// request goes out; never persisted or shown as if it were exact usage.
///
/// Spiked once against the real (non-mock) gateway configured in dev,
/// rather than left as a pure assumption: two consecutive real turns'
/// `usage.input_tokens` differ by (previous turn's real `output_tokens` +
/// whatever new message content was added) — isolating that second term
/// for an 800-character message gave ~211 real tokens against this
/// function's 200-token estimate, ~3.8 real chars/token vs. the 4.0
/// assumed here. Close enough not to revisit without a reason to — and
/// that one data point (repeated single characters) is a harder case for
/// a tokenizer than real prose, which should compress to *more*
/// chars/token, not fewer, so if anything this errs slightly generous
/// already. Same method (diff two real `get_context_usage` reads, subtract
/// the earlier one's `output_tokens`) works to re-check this later against
/// a real, varied conversation if the constant is ever in question.
#[cfg(feature = "server")]
pub fn estimate_tokens(blocks: &[anthropic::ContentBlock]) -> u64 {
    let chars: usize = blocks
        .iter()
        .map(|block| match block {
            anthropic::ContentBlock::Text { text } => text.len(),
            anthropic::ContentBlock::ToolResult { content, .. } => content.len(),
            anthropic::ContentBlock::ToolUse { input, .. } => input.to_string().len(),
            anthropic::ContentBlock::Thinking {
                thinking,
                signature,
            } => thinking.len() + signature.len(),
            anthropic::ContentBlock::CompactionSummary { summary, .. } => summary.len(),
            anthropic::ContentBlock::CompactionPlaceholder { text } => text.len(),
        })
        .sum();
    (chars / 4) as u64
}

/// The size of request that triggers a compaction: the context window less
/// what's kept for the reply (model output allowance + a safety buffer) —
/// opencode's own trigger mechanism, checked *before* a request is sent
/// rather than reacting to a real rejection. See SME-18. Once compacting
/// can't help, `CompactionProgress` moves it nearer the window.
#[cfg(feature = "server")]
pub fn compaction_ceiling(context_window: u32) -> u64 {
    let reserved = MIN_REPLY_TOKENS.max(COMPACTION_SAFETY_BUFFER) as u64;
    (context_window as u64).saturating_sub(reserved)
}

/// The next request's input as the last real usage measured it, plus an
/// estimate of what's new since: everything the last call read and wrote.
/// `None` when nothing has been measured since the history last changed
/// shape (a first request, or one just after a compaction).
#[cfg(feature = "server")]
pub fn projected_input(last_usage: Option<&anthropic::TokenUsage>, new_content_estimate: u64) -> Option<u64> {
    let last_usage = last_usage?;
    let already_used = (last_usage.input_tokens
        + last_usage.output_tokens
        + last_usage.cache_creation_input_tokens
        + last_usage.cache_read_input_tokens)
        .max(0) as u64;
    Some(already_used + new_content_estimate)
}

/// An estimate of a whole request's input: its messages, system prompt
/// and tool definitions, at `estimate_tokens`' 4 characters a token. For a
/// request no usage measured (`projected_input`).
#[cfg(feature = "server")]
pub fn estimate_request_tokens(request: &anthropic::CreateMessageRequest) -> u64 {
    let messages: u64 = request.messages.iter().map(|m| estimate_tokens(&m.content)).sum();
    let system = request.system.as_ref().map_or(0, |s| s.len() / 4) as u64;
    let tools = serde_json::to_string(&request.tools).map_or(0, |t| t.len() / 4) as u64;
    messages + system + tools
}

/// `budget` held to the room a request of `projected_input` tokens really
/// leaves, for one past the trigger because compacting can't help: the
/// floor in `reply_budget` counts on the trigger having kept that room, and
/// a reply it doesn't have is a refusal (SME-386 M3).
#[cfg(feature = "server")]
pub fn budget_within_room(budget: ReplyBudget, context_window: u32, projected_input: u64) -> ReplyBudget {
    let room = (context_window as u64)
        .saturating_sub(projected_input)
        .saturating_sub(COMPACTION_SAFETY_BUFFER as u64);
    let room = u32::try_from(room).unwrap_or(u32::MAX).max(1);
    if room < budget.tokens {
        ReplyBudget { tokens: room, limit: crate::api::chat::ReplyLimit::RoomLeft }
    } else {
        budget
    }
}

/// How many tokens a reply may use (SME-111): the smallest of the model's
/// output cap, where known; the room the request leaves in the window,
/// less `COMPACTION_SAFETY_BUFFER`; and half the window, so a single reply
/// can't crowd the rest of the history out of the next compaction. Never
/// below `MIN_REPLY_TOKENS` (or half a smaller window), as every turn
/// asked for before; the compaction trigger keeps that much room. A request
/// let past the trigger gets `budget_within_room` on top, which can be less.
#[cfg(feature = "server")]
pub fn reply_budget(context_window: u32, projected_input: u64, output_cap: Option<u32>) -> ReplyBudget {
    let half = context_window / 2;
    let room = (context_window as u64)
        .saturating_sub(projected_input)
        .saturating_sub(COMPACTION_SAFETY_BUFFER as u64);
    let room = u32::try_from(room).unwrap_or(u32::MAX);
    let budget = room.min(half).max(MIN_REPLY_TOKENS.min(half));
    let tokens = output_cap.map_or(budget, |cap| budget.min(cap)).max(1);
    let limit = if output_cap == Some(tokens) {
        crate::api::chat::ReplyLimit::OutputCap
    } else if tokens == half {
        crate::api::chat::ReplyLimit::HalfWindow
    } else {
        // The room left, or the floor kept when there's even less.
        crate::api::chat::ReplyLimit::RoomLeft
    };
    ReplyBudget { tokens, limit }
}

/// A reply's budget, and what bound it (for a cut-off notice).
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyBudget {
    pub tokens: u32,
    pub limit: crate::api::chat::ReplyLimit,
}
