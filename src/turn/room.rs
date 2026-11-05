//! What a turn decides about the room a reply gets, as plain functions of
//! `CompactionProgress` and sizes (SME-386 review). The turn's own steps
//! (`build_request_with_room`, `overflow::recover`) call these, and so does
//! the property test in `tests/escape.rs`, which drives them through long
//! random sequences without a database.

use super::*;

/// What a turn does with a request it has built, before sending it.
#[derive(Debug, PartialEq)]
pub(super) enum BeforeSending {
    /// Send it, with this budget.
    Send(ReplyBudget),
    /// Compact, then build it again.
    Compact,
    /// Don't: the turn ends, for this reason.
    Refuse(String),
}

/// A request's reply budget: `reply_budget`, held to the room left when
/// `room_only`.
pub(super) fn budget_for(context_window: u32, projected: u64, output_cap: Option<u32>, room_only: bool) -> ReplyBudget {
    let budget = reply_budget(context_window, projected, output_cap);
    if room_only { budget_within_room(budget, context_window, projected) } else { budget }
}

/// Whether a request's budget is what's left of the room, and too little.
fn under_the_floor(room_only: bool, budget: ReplyBudget) -> bool {
    room_only && budget.limit == crate::api::chat::ReplyLimit::RoomLeft && budget.tokens < MIN_USEFUL_REPLY
}

/// The floor (`MIN_USEFUL_REPLY`), for a request of about `size` tokens
/// built with `budget`. Under it, a request that hasn't just compacted
/// (`compacted`) compacts, whatever got it there (a big paste, or the usage
/// row a compaction clears until a reply lands; SME-386 review M-A). One
/// that has is settled by `CompactionProgress::floor_fired`.
pub(super) fn before_sending(
    progress: &mut CompactionProgress,
    model: &str,
    context_window: u32,
    output_cap: Option<u32>,
    room_only: bool,
    budget: ReplyBudget,
    size: u64,
    compacted: bool,
) -> BeforeSending {
    if !under_the_floor(room_only, budget) {
        return BeforeSending::Send(budget);
    }
    if !compacted {
        return BeforeSending::Compact;
    }
    match progress.floor_fired(size, model, context_window) {
        FloorAnswer::Believe(after) => BeforeSending::Send(budget_for(context_window, after, output_cap, true)),
        FloorAnswer::TryAnyway => BeforeSending::Send(ReplyBudget { tokens: MIN_USEFUL_REPLY, limit: crate::api::chat::ReplyLimit::RoomLeft }),
        FloorAnswer::NoRoom(why) => BeforeSending::Refuse(why),
    }
}

/// What a refusal naming the window (`context_window`, already learned)
/// has the turn do, with `alone` the size no compaction can shrink and
/// `input` the refused request's.
#[derive(Debug, PartialEq)]
pub(super) enum NamedRefusal {
    /// Not even a short reply fits beside the system prompt and tools.
    TooSmall(String),
    /// Send again: compacted first if `compact`.
    Retry { compact: bool },
}

pub(super) fn after_named_refusal(model: &str, context_window: u32, alone: u64, input: u64) -> NamedRefusal {
    let window = u64::from(context_window);
    // Can't help: what no compaction can take away already leaves this
    // window no room to answer in, even a short reply, which is all a
    // request gets once compacting can't help (SME-386 review M-B). Without
    // the estimate's buffer: inside it, the floor (`before_sending`) answers,
    // and lets the user's next send try (final review, the property test).
    if alone + u64::from(MIN_USEFUL_REPLY) > window {
        return NamedRefusal::TooSmall(super::overflow::too_small(model, context_window, alone));
    }
    // Budget only when the input fits with the budget's usual reply: then
    // the budget was the whole problem, and the same history goes again.
    let needs = u64::from(MIN_REPLY_TOKENS) + u64::from(COMPACTION_SAFETY_BUFFER);
    NamedRefusal::Retry { compact: input.saturating_add(needs) > window }
}

/// The window a refusal naming none has the turn assume, for this turn
/// alone: three quarters of the refused request's size, when that's less
/// than it believes. Provider-supplied sizes are saturating here and
/// upstream: `anthropic::overflow` drops implausible ones (SME-73 review).
pub(super) fn guessed_window(input: u64, believed: u32) -> Option<u32> {
    let assumed = u32::try_from(input.saturating_mul(3) / 4).unwrap_or(u32::MAX);
    (assumed < believed).then_some(assumed)
}
