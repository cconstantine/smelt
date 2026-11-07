//! What a turn does with a refusal meaning "this didn't fit" (SME-73):
//! learn the window the server named, shrink what it sends, and send
//! once more. Reading a refusal is `anthropic::overflow`'s business;
//! this is the answering. At most one recovery per request — a later
//! tool round starts its own — and a second refusal ends the turn. The
//! gentler half lives here too: a reply the *window* stopped rather than
//! its budget is kept as it stands and leaves its measured size behind.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

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

    let (mut compacted, hold_to_room) = match named {
        Some(_) => {
            let alone = estimate_request_tokens(&without_messages(refused));
            match after_named_refusal(&turn_model.model, turn_model.context_window, alone, input) {
                NamedRefusal::TooSmall(why) => {
                    // Wakes say so after this without calling.
                    with_compaction_progress(conversation_id, |p| p.too_small(alone, turn_model.context_window));
                    return Err(ServerFnError::new(why));
                }
                // A window named is a window to keep to: the retry gets no
                // more than the room it leaves, whatever's been learned, or
                // the usual reply would be refused again on every send
                // (final review L-1).
                NamedRefusal::Retry { compact } => (compact, true),
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
            (true, false)
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
    // measured, so the rebuild projects from nothing instead. Without one,
    // the budget comes from the larger of the last usage's projection and
    // what the server counted: from the usage alone it would be the refused
    // request's budget again whenever something the usage doesn't see (the
    // tools, the system prompt) grew (SME-386 final review, the property
    // test).
    let projected = projected_input(last_known_usage.as_ref(), estimate_tokens(pending_new_content));
    let mut usage = (!compacted).then(|| anthropic::TokenUsage {
        input_tokens: i64::try_from(projected.map_or(input, |projected| projected.max(input))).unwrap_or(i64::MAX),
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    });
    #[cfg(test)]
    test_hooks::before_retry(conversation_id).await;
    // Keeps to the room as the refused request did (round 2 L1), unless this
    // refusal changed the window or a guess is in force, or as `hold_to_room`
    // says; and to the floor (review L-2), which compacts a retry that didn't,
    // counting it as answering this refusal (final review L-3).
    let reason = refused_reason(named);
    // The estimate fell short of what the server counted by this much: the
    // retry's is taken to, or its budget would be refused again on every
    // send (SME-386 final review, the property test).
    let undercount = overflow.input_tokens.map_or(0, |input| input.saturating_sub(estimate_request_tokens(refused)));
    let (mut request, budget) = build_request_with_room(
        pool,
        conversation_id,
        turn_model,
        slot,
        &mut usage,
        &[],
        &mut compacted,
        hold_to_room,
        Some(&reason),
        undercount,
    )
    .await?;
    // A Claude Code call never reports an overflow (SME-430), so this
    // retry is always an HTTP one, and has no CLI run to use.
    let mut no_cli = crate::claude_cli::run::TurnCli::default();
    match call_model(pool, conversation_id, turn_model, &mut request, slot, &mut no_cli).await {
        Ok((turn, recorder)) => {
            // The server took it (final review L-2). Not a step taken for the
            // refusal count, which a server refusing every step needs (review L2).
            with_compaction_progress(conversation_id, CompactionProgress::retry_taken);
            Ok(Recovered { turn, recorder, budget, compacted })
        }
        // Refused again: the turn fails — but a window this second
        // refusal names is still learned, so the next turn starts right.
        Err(CallError::Overflow(again)) => {
            with_compaction_progress(conversation_id, |p| p.refused(again.input_tokens));
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

/// How close to the server's reported per-request window a window stop's
/// measurement must come for the stop to plausibly have filled that
/// window (SME-439): it absorbs llama.cpp's per-slot rounding and
/// usage-count jitter, and it is small next to any real window. Equal to
/// `COMPACTION_SAFETY_BUFFER` by taste; its own number.
const WINDOW_STOP_MARGIN: i64 = 4_096;

/// A stop that wrote less than this is "very short": it measured almost
/// nothing of the room (below the 64-token slack usage rounding lives
/// under). 32 and not 355 on purpose: a genuine fill can stop with only
/// a little room left (SME-73's W1 story); the floor only rejects
/// near-empty replies (SME-439).
const MIN_WINDOW_STOP_OUTPUT: i64 = 32;

/// How many of a reply's last tokens the repetition-loop tell reads, and
/// how many of them one token must fill to be a loop (SME-439): the
/// incident's reply babbled `…finish.finish.finish…` to its stop.
/// Documented misses: unspaced CJK loops, whole repeated code lines with
/// distinct content, loops shorter than the tail — acceptable because
/// the proximity rule is the primary guard and this only vetoes.
const DEGENERATE_TAIL_TOKENS: usize = 40;
const DEGENERATE_REPEATS: usize = 24;

/// What a request to one of a llama.cpp server's slots gets is its
/// `n_ctx` split across its slots (`n_ctx_slot = n_ctx / total_slots` in
/// llama.cpp's own server code); every other kind reports per model, so
/// its window passes whole. An uneven split (`--slot-context`, upstream
/// proposal) breaks the even division — the margin and the repeat rule
/// are the guard. `None` reported stays `None`. (SME-439 PR 2:
/// `providers::store_details` divides the same way before a report may
/// forget a learned window.)
pub(crate) fn reported_per_request(
    reported: Option<i32>,
    kind: crate::providers::ProviderKind,
    total_slots: Option<u32>,
) -> Option<i64> {
    let reported = i64::from(reported.filter(|w| *w > 0)?);
    if kind == crate::providers::ProviderKind::LlamaCpp {
        if let Some(slots) = total_slots.filter(|s| *s > 1) {
            return Some(reported / i64::from(slots));
        }
    }
    Some(reported)
}

/// Whether a window stop's measured size plausibly *is* the reported
/// per-request window: within [`WINDOW_STOP_MARGIN`] of it. Nothing
/// reported proves nothing (SME-439).
pub(super) fn stop_reads_plausible(measured: i64, reported: Option<i64>) -> bool {
    reported.is_some_and(|w| measured.abs_diff(w) <= WINDOW_STOP_MARGIN as u64)
}

/// The repetition-loop tell: of the reply's last [`DEGENERATE_TAIL_TOKENS`]
/// whitespace-or-punctuation-delimited tokens, one appears
/// [`DEGENERATE_REPEATS`] times. A token scan, not a word scan, so the
/// incident's periods split `finish.finish.finish` into tokens.
pub(super) fn looks_degenerate(text: &str) -> bool {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|t| !t.is_empty())
        .collect();
    let tail = &words[words.len().saturating_sub(DEGENERATE_TAIL_TOKENS)..];
    let mut counts: HashMap<&str, usize> = HashMap::new();
    tail.iter().copied().any(|t| {
        let n = counts.entry(t).or_insert(0);
        *n += 1;
        *n >= DEGENERATE_REPEATS
    })
}

/// The reply's text, for the degenerate tell — thinking and tool blocks
/// say nothing a loop of words would repeat.
fn reply_text(content: &[anthropic::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One `window_learn` span per learning *decision* (SME-439's Expected
/// 6): a records-carrier, ended on the spot, never a duration. Every
/// field is a number, an id, or a fixed enum — no reply text, no model
/// output, no URLs (SME-137's rule) — so the incident can be read off a
/// trace without having sat in the conversation.
#[allow(clippy::too_many_arguments)]
fn trace_window_decision(
    conversation_id: i64,
    model: &str,
    source: crate::providers::WindowSource,
    decision: &str,
    value: u32,
    usage: Option<&anthropic::TokenUsage>,
    reported: Option<i64>,
    reported_per_request: Option<i64>,
) {
    let (measured_input, measured_output, cache_creation, cache_read) = match usage {
        Some(u) => (
            Some(u.input_tokens),
            Some(u.output_tokens),
            Some(u.cache_creation_input_tokens),
            Some(u.cache_read_input_tokens),
        ),
        None => (None, None, None, None),
    };
    tracing::info_span!(
        "window_learn",
        conversation_id,
        smelt.model = model,
        smelt.window.source = source.as_str(),
        smelt.window.decision = decision,
        smelt.window.value = i64::from(value),
        smelt.window.measured_input = measured_input,
        smelt.window.measured_output = measured_output,
        smelt.window.cache_creation = cache_creation,
        smelt.window.cache_read = cache_read,
        smelt.window.reported = reported,
        smelt.window.reported_per_request = reported_per_request,
    )
    .in_scope(|| {});
}

/// The repeat evidence for an ambiguous, implausible window stop
/// (SME-439): the last candidate measurement per `(provider, model)` —
/// its size and the turn that measured it — so a second like-sized stop,
/// two independent fills at one size, can learn what one never may. A
/// repeat only counts from a *later* turn (round 1 M1): one turn can
/// stop twice (its `carries_on` continuation calls the model again),
/// and a looping reply restopping within its own turn — this incident's
/// own shape — measures the same conversation filling, not a second
/// fill. Process-local: a restart drops it, which errs toward never
/// learning (Trade-offs). Never held across an await.
static WINDOW_STOP_CANDIDATES: LazyLock<Mutex<HashMap<(i64, String), (i64, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Numbers telling one turn's model calls from every other turn's
/// (SME-439 round 1 M1): `run_turn_steps` takes the next one before its
/// loop, so every model call of that turn — a `carries_on` continuation
/// included — shares it and no other turn has it.
static WINDOW_STOP_TURNS: AtomicU64 = AtomicU64::new(0);

/// The number this turn's window-stop evidence carries; take it once per
/// turn, before its loop.
pub(super) fn next_window_stop_turn() -> u64 {
    WINDOW_STOP_TURNS.fetch_add(1, Ordering::Relaxed)
}

/// What this model's server reports per request: the stored reported
/// window and its llama.cpp slot split (two queries — the learner's own
/// rare path, only consulted when a stop is ambiguous and this turn must
/// decide what it measured).
async fn reported_per_request_for(
    pool: &PgPool,
    turn_model: &crate::providers::TurnModel,
) -> (Option<i64>, Option<i64>) {
    let reported_i32 = db::get_provider_model(pool, turn_model.provider_id, &turn_model.model)
        .await
        .ok()
        .flatten()
        .and_then(|row| row.reported_context_window)
        .filter(|w| *w > 0);
    let reported = reported_i32.map(i64::from);
    let total_slots = if turn_model.kind == crate::providers::ProviderKind::LlamaCpp {
        db::get_inference_provider(pool, turn_model.provider_id)
            .await
            .ok()
            .flatten()
            .and_then(|provider| crate::providers::server_caps(&provider))
            .and_then(|caps| caps.total_slots)
    } else {
        None
    };
    (reported, reported_per_request(reported_i32, turn_model.kind, total_slots))
}

/// What a window stop measured is the room the server really had: the
/// whole call — input, cache and output together — stopped at the
/// window. But one ambiguous stop measures the *conversation* too (the
/// incident: a looping reply stopped at 355 tokens and saved the
/// conversation's 160,799 as strata's window for 1.7 hours). So
/// SME-439's gate: save when the stop is unambiguous (the server's own
/// `model_context_window_exceeded`) or the measurement lands within
/// [`WINDOW_STOP_MARGIN`] of what the server reports per request, or a
/// like-sized stop comes in a later turn (a second stop in this same
/// turn is the same fill counted twice); never from a degenerate or
/// near-empty reply, and never below [`MIN_PLAUSIBLE_WINDOW`]. Reported
/// and disagreeing: save nothing and leave this turn's belief where the
/// server said it is; reported unknown: evidence for this turn only. The
/// reply is kept either way and this retries nothing itself; the notice
/// wording (`window_stop`) is untouched.
pub(super) async fn learn_window_stop(
    pool: &PgPool,
    conversation_id: i64,
    stop_turn: u64,
    turn_model: &mut crate::providers::TurnModel,
    usage: &anthropic::TokenUsage,
    content: &[anthropic::ContentBlock],
    stop_reason: &str,
) {
    let measured = usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens);
    let window = u32::try_from(measured).unwrap_or(u32::MAX);
    if usage.output_tokens < MIN_WINDOW_STOP_OUTPUT {
        trace_window_decision(conversation_id, &turn_model.model, crate::providers::WindowSource::WindowStop, "ignored_short", window, Some(usage), None, None);
        return;
    }
    if looks_degenerate(&reply_text(content)) {
        trace_window_decision(conversation_id, &turn_model.model, crate::providers::WindowSource::WindowStop, "ignored_degenerate", window, Some(usage), None, None);
        return;
    }
    if measured < MIN_PLAUSIBLE_WINDOW {
        trace_window_decision(conversation_id, &turn_model.model, crate::providers::WindowSource::WindowStop, "not_lower", window, Some(usage), None, None);
        return;
    }
    // The server's own claim the window filled skips the proximity rule.
    let unambiguous = stop_reason == "model_context_window_exceeded";
    let (reported, per_request) = reported_per_request_for(pool, turn_model).await;
    let key = (turn_model.provider_id, turn_model.model.clone());
    let save = unambiguous || stop_reads_plausible(measured, per_request);
    let repeats = {
        let mut candidates = WINDOW_STOP_CANDIDATES.lock().unwrap_or_else(|e| e.into_inner());
        if save {
            candidates.remove(&key); // a save ends the argument
            true
        } else if candidates.get(&key).is_some_and(|(prev, prev_turn)| *prev_turn != stop_turn && prev.abs_diff(measured) <= WINDOW_STOP_MARGIN as u64) {
            candidates.remove(&key); // a later turn's stop is the repeat itself
            true
        } else {
            // First candidate, or this turn's own stop again: keep (or
            // update) it, still only a repeat for a later turn.
            candidates.insert(key, (measured, stop_turn));
            false
        }
    };
    if repeats {
        learn_as(pool, conversation_id, turn_model, Some(window), crate::providers::WindowSource::WindowStop, Some(usage), reported, per_request).await;
        return;
    }
    if per_request.is_some() {
        // The server said what it has; this stop measured the
        // conversation. The turn's belief — the meter, the next
        // compaction check — stays put; the candidate keeps the repeat
        // evidence for a second stop.
        trace_window_decision(conversation_id, &turn_model.model, crate::providers::WindowSource::WindowStop, "candidate", window, Some(usage), reported, per_request);
        return;
    }
    // Nothing reported: one stop is evidence for this turn only (the
    // meter and this turn's compaction read it; nobody else ever does).
    turn_model.context_window = turn_model.context_window.min(window);
    tracing::info!(conversation_id, model = %turn_model.model, window, "window stop believed for this turn only: the server reports no context window");
    trace_window_decision(conversation_id, &turn_model.model, crate::providers::WindowSource::WindowStop, "turn_only", window, Some(usage), reported, per_request);
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

/// Clears the window-stop repeat candidates (SME-439). Every
/// `#[sqlx::test]` database hands out the same provider ids while the
/// tests run in one binary, and candidates are keyed by those ids, so a
/// test that reads or writes them starts by forgetting them;
/// `lock_turn_tests` keeps the tests off each other's turns.
#[cfg(test)]
pub(super) fn forget_window_stop_candidates() {
    WINDOW_STOP_CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
        learn_as(pool, conversation_id, turn_model, Some(window), crate::providers::WindowSource::OllamaPs, Some(usage), None, None).await;
    }
}

/// Puts `window` on the turn's copy (every later size reads it), saves it
/// for the model as `source`, and traces the decision (SME-439). A save
/// that fails — a provider moved mid-turn — is not the turn's failure:
/// it warns and the turn proceeds on its copy, as ever. Returns whether
/// a window was named at all.
#[allow(clippy::too_many_arguments)]
pub(super) async fn learn_as(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    window: Option<u32>,
    source: crate::providers::WindowSource,
    usage: Option<&anthropic::TokenUsage>,
    reported: Option<i64>,
    reported_per_request: Option<i64>,
) -> Option<u32> {
    let window = window?;
    if window < turn_model.context_window {
        turn_model.context_window = window;
    }
    let decision = match crate::providers::learn_context_window(pool, turn_model, window, source, conversation_id).await {
        Ok(true) => {
            tracing::info!(conversation_id, model = %turn_model.model, source = source.as_str(), window, "learned a smaller context window");
            "saved"
        }
        Ok(false) => "not_lower",
        Err(e) => {
            tracing::warn!(conversation_id, "couldn't save the context window a learner settled on: {e}");
            "write_failed"
        }
    };
    trace_window_decision(conversation_id, &turn_model.model, source, decision, window, usage, reported, reported_per_request);
    Some(window)
}

/// A refusal's learner: [`learn_as`] with the refusal as its source. The
/// 4-argument shape stays because `compaction.rs` calls it (SME-431
/// owns that file; this signature is the overlap guard).
pub(super) async fn learn(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    window: Option<u32>,
) -> Option<u32> {
    learn_as(pool, conversation_id, turn_model, window, crate::providers::WindowSource::Refusal, None, None, None).await
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
