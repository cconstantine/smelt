//! A model call from a turn: streaming it with the Ollama retries, and
//! recording its usage.

use super::*;

/// Ollama's Anthropic-compatibility shim can fail to turn a model's raw
/// output into a valid tool call, surfacing as a flat 500 with a message
/// like `error parsing tool call: raw='...', err=...` instead of streaming
/// normally. Two different root causes share this exact shape (at least
/// for `gpt-oss`-family models): thinking's reasoning landing in the same
/// text the shim expected pure tool-call JSON in (see `thinking_enabled`'s
/// doc comment), or the model just writing invalid JSON on its own — e.g.
/// a bare `?` for a value it wasn't sure how to fill in. Real Anthropic
/// never returns this. Deliberately narrow (a distinctive, Ollama-specific
/// phrase, not just "any 500") so an unrelated upstream failure doesn't
/// silently double latency/cost by retrying pointlessly.
#[cfg(feature = "server")]
pub(super) fn is_ollama_thinking_tool_call_corruption(message: &str) -> bool {
    message.contains("error parsing tool call")
}

/// Extra attempts (beyond the first) for the failure
/// `is_ollama_thinking_tool_call_corruption` recognizes — the first retry
/// also drops `thinking` (see the call site), remaining retries are plain
/// regenerations, since a local model's next sampling pass often just
/// doesn't repeat the same malformed JSON. Bounded so a call the model is
/// reliably bad at can't retry forever.
#[cfg(feature = "server")]
pub(super) const TOOL_CALL_PARSE_RETRIES: usize = 2;

/// Calls the model, and every tab watching streams the reply: the text so
/// far is kept for a tab that connects mid-reply, and each delta published.
///
/// See `is_ollama_thinking_tool_call_corruption` — not always
/// caused by thinking specifically (a local model can just
/// flub a tool call's JSON on its own, e.g. writing a bare `?`
/// for a value it wasn't sure about), so the mitigation is two
/// parts: drop `thinking` once (a plausible contributing
/// factor, and free to try), then fall back to plain retries —
/// regenerating is often enough on its own since sampling
/// varies run to run. Bounded so a model that's reliably bad at
/// one particular call can't loop forever.
#[cfg(feature = "server")]
pub(super) async fn call_model(
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &mut anthropic::CreateMessageRequest,
) -> ServerFnResult<anthropic::stream::StreamedTurn> {
    let mut relay = |delta: &str| relay_reply_delta(conversation_id, delta);
    let mut last_err = String::new();
    for attempt in 0..=TOOL_CALL_PARSE_RETRIES {
        // A fresh bubble per model call, and per retry: a retry's
        // text replaces a failed attempt's.
        clear_reply_in_progress(conversation_id);
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::ReplyReset {},
        );
        match anthropic::stream::stream_anthropic_message(
            &turn_model.endpoint,
            request,
            &mut relay,
        )
        .await
        {
            Ok(t) => return Ok(t),
            Err(e)
                if attempt < TOOL_CALL_PARSE_RETRIES
                    && is_ollama_thinking_tool_call_corruption(&e) =>
            {
                request.thinking = None;
                request.chat_template_kwargs = turn_model.chat_template_kwargs(false);
                last_err = e;
            }
            Err(e) => return Err(ServerFnError::new(e)),
        }
    }
    Err(ServerFnError::new(last_err))
}

/// Records a model call's usage and cost.
///
/// Real usage from this call is the ground truth for "how much
/// context is actually being used" — persisted so
/// `get_context_usage` survives a reload, published live so the
/// indicator updates without one, and the caller tracks it as the new
/// baseline the *next* compaction check starts from. See
/// SME-18.
/// Also appended to the conversation's call history, in the
/// same transaction, for its cost (SME-106).
#[cfg(feature = "server")]
pub(super) async fn record_call(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    turn: &anthropic::stream::StreamedTurn,
) -> ServerFnResult<()> {
    db::record_model_call(
        pool,
        &db::ModelCall {
            conversation_id,
            provider_id: turn_model.provider_id,
            model: &turn_model.model,
            kind: db::ModelCallKind::Turn,
            usage: turn.usage,
            cost_usd: crate::pricing::call_cost(
                turn_model.price_catalog_provider.as_deref(),
                &turn_model.model,
                &turn.usage,
            ),
            finished: true,
        },
    )
    .await
    .map_err(ServerFnError::new)?;
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::ContextUsageUpdate {
            usage: turn.usage,
            context_window: turn_model.context_window,
        },
    );
    Ok(())
}
