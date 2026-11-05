//! What a turn does with a refusal meaning "this didn't fit" (SME-73):
//! learn the window the server named, shrink what it sends, and send
//! once more. Reading a refusal is `anthropic::overflow`'s business;
//! this is the answering. At most one recovery per request — a later
//! tool round starts its own — and a second refusal ends the turn. The
//! gentler half lives here too: a reply the *window* stopped rather than
//! its budget is kept as it stands and leaves its measured size behind.

use super::*;

/// What a recovery ends with: the retry's reply and recorder, the budget
/// of the request that got it — the one `save_stop_notice` reads — and
/// whether it compacted.
pub(super) struct Recovered {
    pub(super) turn: anthropic::stream::StreamedTurn,
    pub(super) recorder: CallRecorder,
    pub(super) budget: ReplyBudget,
    pub(super) compacted: bool,
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
    let believed = turn_model.context_window;

    let compacted = match named {
        Some(_) => {
            let alone = estimate_request_tokens(&without_messages(refused));
            match after_named_refusal(&turn_model.model, turn_model.context_window, alone, input) {
                NamedRefusal::TooSmall(why) => return Err(ServerFnError::new(why)),
                NamedRefusal::Retry { compact } => compact,
            }
        }
        None => {
            // The assumption for this turn's compaction and retry. A guess,
            // so it never leaves the turn; and with no window named, the
            // history is what has to go.
            if let Some(assumed) = guessed_window(input, believed) {
                turn_model.context_window = assumed;
                with_compaction_progress(conversation_id, |p| p.guessed(believed, assumed));
            }
            true
        }
    };
    if compacted {
        #[cfg(test)]
        test_hooks::before_summary_call(conversation_id).await;
        compact_conversation(pool, conversation_id, turn_model, slot, Some(refused_reason(named))).await?;
        // Counted here, not by the caller: a retry refused too ends the turn
        // without returning to it, and every wake would pay for another
        // summary (SME-386 round 2 M4).
        with_compaction_progress(conversation_id, |p| p.refusal_compacted(turn_model.context_window));
    }

    // Once more. A compaction summarized the history the old usage
    // measured, so the rebuild projects from nothing instead.
    let mut usage = (!compacted).then_some(last_known_usage).flatten();
    #[cfg(test)]
    test_hooks::before_retry(conversation_id).await;
    // Keeps to the room as the refused request did (round 2 L1), unless this
    // refusal changed the window or a guess is in force, and to the floor
    // (review L-2). The floor takes no further compaction here: only a request
    // that just compacted can be under it.
    let (mut request, budget) =
        build_request_with_room(pool, conversation_id, turn_model, slot, &mut usage, pending_new_content, &mut true).await?;
    match call_model(pool, conversation_id, turn_model, &mut request, slot).await {
        Ok((turn, recorder)) => Ok(Recovered { turn, recorder, budget, compacted }),
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

/// How far short of its budget a `max_tokens` stop may be written before
/// the stop stops being the budget's: usage counts round, and a llama.cpp
/// slot that fills stops with whatever room the window left — far less
/// than the budget it was sent with (SME-73).
const WINDOW_STOP_SLACK: i64 = 64;

/// The smallest measured size worth learning: below this the stop tells
/// more about a server that reports no usage than about any window — the
/// same floor `anthropic::overflow::classify` refuses small windows at.
const MIN_PLAUSIBLE_WINDOW: i64 = 1_024;

/// Whether this stop means the window ran out rather than the reply
/// budget (SME-73): Claude 4.5+ says so itself (`model_context_window_exceeded`,
/// and a gateway passes it through). The `max_tokens` tell is
/// llama.cpp's alone — anything but a stop word maps to `max_tokens`
/// there, so a stop written more than [`WINDOW_STOP_SLACK`] under the
/// budget is the slot, not the budget. An Anthropic-kind server means
/// `max_tokens` literally, and Ollama truncates quietly rather than
/// stopping short (that tell is `learn_ollama_window`'s), so there the
/// stop stays what it says. A server that reported no usage at all
/// (`written` 0) keeps the budget reading: nothing says otherwise.
pub(super) fn window_stop(kind: crate::providers::ProviderKind, stop_reason: &str, written: i64, budget: u32) -> bool {
    match stop_reason {
        "model_context_window_exceeded" => true,
        "max_tokens" => kind == crate::providers::ProviderKind::LlamaCpp && written > 0 && written + WINDOW_STOP_SLACK < i64::from(budget),
        _ => false,
    }
}

/// What a window stop measured is the room the server really had: the
/// whole call — input, cache and output together — stopped at the window.
/// When that came in below this turn's believed window by more than
/// `COMPACTION_SAFETY_BUFFER` the belief was too big: learn the measured
/// size (the write only ever lowers), and the next request compacts
/// before it is refused. The reply is kept either way and this retries
/// nothing itself; a save that doesn't land says nothing more (`learn`).
pub(super) async fn learn_window_stop(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    usage: &anthropic::TokenUsage,
) {
    let measured = usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens);
    if measured >= MIN_PLAUSIBLE_WINDOW
        && measured.saturating_add(i64::from(COMPACTION_SAFETY_BUFFER)) < i64::from(turn_model.context_window)
    {
        let window = u32::try_from(measured).unwrap_or(u32::MAX);
        learn(pool, conversation_id, turn_model, Some(window)).await;
    }
}

/// The floor on asking Ollama: below this an estimate-vs-measured gap is
/// likelier tokeniser slop than a truncated prompt (SME-73).
const OLLAMA_CHECK_MIN_ESTIMATE: u64 = 8_192;

/// Whether a reply measuring `measured` to a request estimated at
/// `estimate` reads as quietly truncated: the request big enough for the
/// gap to mean anything, and the reply below half of it (SME-73).
pub(super) fn ollama_tell_says_truncated(estimate: u64, measured: u64) -> bool {
    estimate >= OLLAMA_CHECK_MIN_ESTIMATE && measured < estimate / 2
}

/// Ollama answers an oversized request quietly truncated rather than
/// refusing it: its prompt builder drops the oldest messages until the
/// prompt fits the loaded `num_ctx`. The tell is `ollama_tell_says_truncated`
/// against this reply; then ask `/api/ps` what window the model is loaded
/// with — that, not the belief, is what the next request must fit — and
/// learn it when it's smaller. The reply already written is kept either
/// way: re-running a quietly truncated answer is not attempted
/// (Trade-offs).
pub(super) async fn learn_ollama_window(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    request: &anthropic::CreateMessageRequest,
    usage: &anthropic::TokenUsage,
) {
    if turn_model.kind != crate::providers::ProviderKind::Ollama {
        return;
    }
    let estimate = estimate_request_tokens(request);
    let measured = usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens)
        .max(0) as u64;
    if !ollama_tell_says_truncated(estimate, measured) {
        return;
    }
    let loaded = match anthropic::models::ollama_loaded_window(&turn_model.endpoint, &turn_model.model).await {
        Ok(loaded) => loaded,
        // The turn answers as if nothing was learned: the reply stands,
        // and the next reply gets the tell again.
        Err(e) => {
            tracing::info!(conversation_id, error = %e, "couldn't ask Ollama what's loaded");
            None
        }
    };
    if let Some(window) = loaded.filter(|window| *window < turn_model.context_window) {
        learn(pool, conversation_id, turn_model, Some(window)).await;
    }
}

/// Puts `window` on the turn's copy (every later size reads it) and
/// saves it for the model. A save that fails — a provider moved mid-
/// turn — is not the turn's failure: it says nothing more. Returns
/// whether a window was named at all. Shared with the compaction's own
/// overflow recovery (`compaction.rs`) and the window-stop measurement
/// below — the plan's learners 3 and 4.
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

/// Why a compaction happened, in the words its transcript divider reads
/// after "Conversation compacted: " (SME-73): the refusal it answered,
/// naming the window that refusal gave when it named one.
pub(super) fn refused_reason(window: Option<u32>) -> String {
    match window {
        Some(window) => format!(
            "the model's server refused a request that didn't fit its {}-token window",
            grouped(u64::from(window))
        ),
        None => "the model's server refused a request that didn't fit".to_string(),
    }
}

/// The failed turn's words when even compaction couldn't make it fit.
fn still_did_not_fit(model: &str, window: Option<u32>, compacted: bool, said: &str) -> String {
    let window = window.map(|w| format!(" ({} tokens)", grouped(u64::from(w)))).unwrap_or_default();
    let compacted = if compacted { " after compacting" } else { "" };
    format!("The conversation still didn't fit `{model}`'s context window{window}{compacted}. The server said: {said}")
}

/// The failed turn's words when the window can't hold the system prompt
/// and tools, leave alone a conversation.
pub(super) fn too_small(model: &str, window: u32, alone: u64) -> String {
    format!(
        "`{model}`'s context window ({} tokens) is too small for smelt's system prompt and tools (about {} tokens). \
         Give the model a larger context on its server, or pick another model.",
        grouped(u64::from(window)),
        grouped(alone)
    )
}
