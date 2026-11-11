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
/// `input` the refused request's, when the refusal counted it (SME-422:
/// a server that counts `max_tokens` and names only its window names no
/// count, and neither does a count too absurd to be believed).
#[derive(Debug, PartialEq)]
pub(super) enum NamedRefusal {
    /// Not even a short reply fits beside the system prompt and tools.
    TooSmall(String),
    /// Send again: compacted first if `compact`. A refusal that named no
    /// input vouches for no room — the estimate of the request it just
    /// refused is all the budget could be sized from — so it never
    /// retries budget-only, and its retry takes only a short reply
    /// (`short_reply`; SME-422).
    Retry { compact: bool, short_reply: bool },
}

pub(super) fn after_named_refusal(model: &str, context_window: u32, alone: u64, input: Option<u64>) -> NamedRefusal {
    let window = u64::from(context_window);
    // Can't help: what no compaction can take away already leaves this
    // window no room to answer in, even a short reply, which is all a
    // request gets once compacting can't help (SME-386 review M-B). Without
    // the estimate's buffer: inside it, the floor (`before_sending`) answers,
    // and lets the user's next send try (final review, the property test).
    if alone + u64::from(MIN_USEFUL_REPLY) > window {
        return NamedRefusal::TooSmall(super::overflow::too_small(model, context_window, alone));
    }
    // With no count named, nothing says the input leaves room for the
    // budget's usual reply — a same-history retry takes the room that was
    // just refused, and is refused again, every send (SME-422's wedge).
    // Compaction is what can shrink the request; and `MIN_USEFUL_REPLY` is
    // both the smallest reply worth sending (the floor's own `TryAnyway`
    // arm sends exactly this) and the spare a compacted request is
    // guaranteed beside it, so whenever any reply fits this one does.
    let Some(input) = input else {
        return NamedRefusal::Retry { compact: true, short_reply: true };
    };
    // Budget only when the input fits with the budget's usual reply: then
    // the budget was the whole problem, and the same history goes again.
    let needs = u64::from(MIN_REPLY_TOKENS) + u64::from(COMPACTION_SAFETY_BUFFER);
    NamedRefusal::Retry { compact: input.saturating_add(needs) > window, short_reply: false }
}

/// Hold a budget to the short reply (SME-422): the smallest the codebase
/// promises is worth sending, the one the floor's `TryAnyway` arm sends —
/// so a retry answering a refusal that named no input takes only room the
/// request is guaranteed to leave whenever it leaves room for anything.
pub(super) fn short_reply_budget(budget: ReplyBudget) -> ReplyBudget {
    ReplyBudget { tokens: budget.tokens.min(MIN_USEFUL_REPLY), limit: crate::api::chat::ReplyLimit::RoomLeft }
}

/// The window a refusal naming none has the turn assume, for this turn
/// alone: three quarters of the refused request's size, when that's less
/// than it believes. Provider-supplied sizes are saturating here and
/// upstream: `anthropic::overflow` drops implausible ones (SME-73 review).
pub(super) fn guessed_window(input: u64, believed: u32) -> Option<u32> {
    let assumed = u32::try_from(input.saturating_mul(3) / 4).unwrap_or(u32::MAX);
    (assumed < believed).then_some(assumed)
}

/// A refused request's estimate shortfall, and what may still be true of a
/// request built after a compaction of it (SME-421): the part the estimate
/// missed sat in the history the summary replaced, so a compacted request
/// is charged only `alone`'s share of it — what no compaction shrinks.
#[derive(Clone, Copy, Debug)]
pub(super) struct Undercount {
    /// What the refusal counted more than the estimate of the request it
    /// refused.
    full: u64,
    /// That refused request's own estimate.
    refused_estimate: u64,
    /// The estimate of what survives every compaction: the refused request
    /// with its messages emptied.
    alone: u64,
}

impl Undercount {
    /// Nothing was refused: a turn's normal steps.
    pub(super) const fn none() -> Self {
        Self { full: 0, refused_estimate: 0, alone: 0 }
    }

    /// The refusal counted `input` tokens if it said so (`None`: no
    /// shortfall is known); `refused_estimate` is the refused request's
    /// estimate and `alone` the part of it no compaction shrinks.
    pub(super) fn from_refusal(input: Option<u64>, refused_estimate: u64, alone: u64) -> Self {
        Self { full: input.map_or(0, |input| input.saturating_sub(refused_estimate)), refused_estimate, alone }
    }

    /// What a request of this kind is charged on top of its own estimate:
    /// the whole shortfall until something is summarized away — and then
    /// only `alone`'s share of it, since the summary replaced the part the
    /// estimate missed. Exact when the estimate's error is uniform across
    /// the request (the SME-386 regime that earned the correction), and
    /// never more than the whole: a `refused_estimate` of 0 (a request
    /// with no system prompt — unreachable today) falls back to the full
    /// correction, so the SME-386 protection is the default.
    pub(super) fn for_request(self, compacted: bool) -> u64 {
        if !compacted || self.refused_estimate == 0 {
            return self.full;
        }
        self.full.saturating_mul(self.alone) / self.refused_estimate
    }
}
