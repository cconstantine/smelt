//! What a turn does with a refusal meaning "this didn't fit" (SME-73):
//! learn the window the server named, shrink what it sends, and send
//! once more. Reading a refusal is `anthropic::overflow`'s business;
//! this is the answering. At most one recovery per request — a later
//! tool round starts its own — and a second refusal ends the turn.

use super::*;

/// What a recovery ends with: the retry's reply and recorder, and the
/// budget of the request that got it — the one `save_stop_notice` reads.
pub(super) struct Recovered {
    pub(super) turn: anthropic::stream::StreamedTurn,
    pub(super) recorder: CallRecorder,
    pub(super) budget: ReplyBudget,
}

/// One recovery from an overflow refusal. `turn_model` is the turn's own
/// snapshot: a window the refusal names lands on it here, so every later
/// size of the turn — the reply budget, the compaction, the usage the
/// meter publishes — reads what the server just said, and it is saved
/// for the model too. With no window named the turn assumes three
/// quarters of the refused request's size, for this turn alone: a bound
/// it keeps to itself and saves for nobody.
pub(super) async fn recover(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    last_known_usage: Option<anthropic::TokenUsage>,
    pending_new_content: &[anthropic::ContentBlock],
    slot: anthropic::stream::SlotPolicy<'_>,
    refused: &anthropic::CreateMessageRequest,
    overflow: &anthropic::overflow::Overflow,
) -> ServerFnResult<Recovered> {
    let named = learn(pool, conversation_id, turn_model, overflow.window).await;
    let input = overflow.input_tokens.unwrap_or_else(|| estimate_request_tokens(refused));
    // What every retry must still leave room for on top of the input:
    // a reply and the estimate's headroom.
    let needs = u64::from(MIN_REPLY_TOKENS) + u64::from(COMPACTION_SAFETY_BUFFER);

    if named.is_some() {
        // Can't help: what no compaction can take away — the system
        // prompt and the tool definitions — already leaves this window
        // no room to answer in. Don't compact; fail at once, and say
        // what to change.
        let alone = estimate_request_tokens(&without_messages(refused));
        if alone + needs > u64::from(turn_model.context_window) {
            return Err(ServerFnError::new(too_small(&turn_model.model, turn_model.context_window, alone)));
        }
    } else {
        // The assumption for this turn's compaction and retry — three
        // quarters of the refused request's size. A guess, so it never
        // leaves the turn.
        let assumed = u32::try_from(input * 3 / 4).unwrap_or(u32::MAX);
        if assumed < turn_model.context_window {
            turn_model.context_window = assumed;
        }
    }

    // Budget only when the input fits the window the turn now holds:
    // then the reply budget was the whole problem, and the same history
    // goes again as it stands. It doesn't, or no window was named:
    // summarize first, with the window now known.
    let compacted = named.is_none() || input + needs > u64::from(turn_model.context_window);
    if compacted {
        #[cfg(test)]
        test_hooks::before_summary_call(conversation_id).await;
        compact_conversation(pool, conversation_id, turn_model, slot).await?;
    }

    // Once more. A compaction summarized the history the old usage
    // measured, so the rebuild projects from nothing instead.
    let usage = (!compacted).then_some(last_known_usage).flatten();
    #[cfg(test)]
    test_hooks::before_retry(conversation_id).await;
    let (mut request, budget) = build_request(pool, conversation_id, turn_model, usage.as_ref(), pending_new_content).await?;
    match call_model(pool, conversation_id, turn_model, &mut request, slot).await {
        Ok((turn, recorder)) => Ok(Recovered { turn, recorder, budget }),
        // Refused again: the turn fails — but a window this second
        // refusal names is still learned, so the next turn starts right.
        Err(CallError::Overflow(again)) => {
            learn(pool, conversation_id, turn_model, again.window).await;
            Err(ServerFnError::new(still_did_not_fit(
                &turn_model.model,
                again.window.or(named),
                compacted,
                &again.message,
            )))
        }
        Err(CallError::Failed(e)) => Err(e),
    }
}

/// Puts `window` on the turn's copy (every later size reads it) and
/// saves it for the model. A save that fails — a provider moved mid-
/// turn — is not the turn's failure: it says nothing more. Returns
/// whether a window was named at all. Shared with the compaction's own
/// overflow recovery (`compaction.rs`), the plan's learner 3.
pub(super) async fn learn(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    window: Option<u32>,
) -> Option<u32> {
    let window = window?;
    if window < turn_model.context_window {
        turn_model.context_window = window;
    }
    if let Err(e) = crate::providers::learn_context_window(pool, turn_model, window).await {
        tracing::warn!(conversation_id, "couldn't save the context window a refusal named: {e}");
    }
    Some(window)
}

/// `request` with its conversation taken out: what's left is the size
/// no compaction can shrink.
fn without_messages(request: &anthropic::CreateMessageRequest) -> anthropic::CreateMessageRequest {
    let mut alone = request.clone();
    alone.messages = vec![];
    alone
}

/// A token count with the thousands commas the SME-73 wordings name
/// windows in ("32,768 tokens"): Rust's own format strings have no such
/// grouping.
pub(super) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The failed turn's words when even compaction couldn't make it fit.
fn still_did_not_fit(model: &str, window: Option<u32>, compacted: bool, said: &str) -> String {
    let window = window.map(|w| format!(" ({} tokens)", grouped(u64::from(w)))).unwrap_or_default();
    let compacted = if compacted { " after compacting" } else { "" };
    format!("The conversation still didn't fit `{model}`'s context window{window}{compacted}. The server said: {said}")
}

/// The failed turn's words when the window can't hold the system prompt
/// and tools, leave alone a conversation.
fn too_small(model: &str, window: u32, alone: u64) -> String {
    format!(
        "`{model}`'s context window ({} tokens) is too small for smelt's system prompt and tools (about {} tokens). \
         Give the model a larger context on its server, or pick another model.",
        grouped(u64::from(window)),
        grouped(alone)
    )
}
