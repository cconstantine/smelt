//! A model call from a turn: streaming it with the Ollama retries, and
//! recording its usage.

use super::*;
#[cfg(feature = "server")]
use std::sync::atomic::{AtomicBool, Ordering};

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
/// Returns the reply with the recorder for its attempt, which the caller
/// `finish`es once the reply is saved; each failed attempt's recorder
/// drops here, recording what that attempt used (SME-178).
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
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &mut anthropic::CreateMessageRequest,
) -> ServerFnResult<(anthropic::stream::StreamedTurn, CallRecorder)> {
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
        let recorder = CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Turn);
        match anthropic::stream::stream_anthropic_message_metered(
            &turn_model.endpoint,
            request,
            &mut relay,
            recorder.meter(),
        )
        .await
        {
            Ok(t) => return Ok((t, recorder)),
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

/// Records one model call attempt's usage and cost (SME-106), however the
/// call ends (SME-178): made before the call, it's `finish`ed with the
/// reply once that's saved, and a recorder dropped before then (a Stop
/// dropping the turn, a stream error, a timeout, a failed save) records
/// what its meter knows instead.
///
/// States: armed (made; the call streams, or its reply is being saved),
/// then either finished (`finish` took it: disarmed, its record spawned)
/// or dropped while armed (its record of `meter.known()` spawned, if
/// anything is known). `finish` disarms before it spawns, so a call is
/// never recorded twice. A drop's record is never the conversation's
/// last-known usage (`ModelCall::finished`) and publishes nothing; it
/// reaches an open context panel on its next fetch. A finished record is
/// current while `finish` is awaited, and stops being current when
/// `finish`'s caller is dropped (a Stop) before it ends (code review 1).
#[cfg(feature = "server")]
pub(super) struct CallRecorder {
    pool: PgPool,
    conversation_id: i64,
    provider_id: i64,
    model: String,
    price_catalog_provider: Option<String>,
    kind: db::ModelCallKind,
    meter: anthropic::stream::UsageMeter,
    armed: bool,
}

#[cfg(feature = "server")]
impl CallRecorder {
    pub(super) fn new(
        pool: &PgPool,
        conversation_id: i64,
        turn_model: &crate::providers::TurnModel,
        kind: db::ModelCallKind,
    ) -> Self {
        Self {
            pool: pool.clone(),
            conversation_id,
            provider_id: turn_model.provider_id,
            model: turn_model.model.clone(),
            price_catalog_provider: turn_model.price_catalog_provider.clone(),
            kind,
            meter: anthropic::stream::UsageMeter::default(),
            armed: true,
        }
    }

    /// The meter the call's stream keeps its usage in.
    pub(super) fn meter(&self) -> &anthropic::stream::UsageMeter {
        &self.meter
    }

    /// The record of `usage`, to write in a spawned task: a finished
    /// call's with the flag saying whether its turn is still current
    /// (`db::record_model_call_if_current`), an unfinished one's with
    /// `None`. Says whether it wrote the last-known usage.
    fn record(
        &self,
        usage: anthropic::TokenUsage,
        current: Option<Arc<AtomicBool>>,
    ) -> impl Future<Output = Result<bool, sqlx::Error>> + Send + 'static {
        let pool = self.pool.clone();
        let (conversation_id, provider_id, kind) = (self.conversation_id, self.provider_id, self.kind);
        let model = self.model.clone();
        let cost_usd = crate::pricing::call_cost(self.price_catalog_provider.as_deref(), &model, &usage);
        async move {
            let finished = current.is_some();
            let call = db::ModelCall { conversation_id, provider_id, model: &model, kind, usage, cost_usd, finished };
            let still_current = || current.is_some_and(|c| c.load(Ordering::SeqCst));
            db::record_model_call_if_current(&pool, &call, still_current).await
        }
    }

    /// Records the finished call `turn`. Real usage from this call is the
    /// ground truth for "how much context is actually being used": a
    /// turn's call (`context_window` given) is persisted as the
    /// conversation's last-known usage, so `get_context_usage` survives a
    /// reload, and published live so the indicator updates without one;
    /// the caller tracks it as the baseline the *next* compaction check
    /// starts from (SME-18). Also appended to the conversation's call
    /// history, in the same transaction, for its cost (SME-106).
    ///
    /// Written in a spawned task the caller awaits: a Stop dropping the
    /// caller mid-record leaves the task to finish it, and the recorder,
    /// already disarmed, doesn't record it again (SME-178). The call is
    /// still counted then, but its usage is the last-known usage, and
    /// published, only if the caller was still there when the record
    /// wrote it: once it's gone the next turn may already have written its
    /// own (code review 1). A failed write fails the caller, as before.
    pub(super) async fn finish(
        mut self,
        turn: &anthropic::stream::StreamedTurn,
        context_window: Option<u32>,
    ) -> ServerFnResult<()> {
        self.armed = false;
        let current = Arc::new(AtomicBool::new(true));
        let _current_while_awaited = NotCurrentOnDrop(current.clone());
        let record = self.record(turn.usage, Some(current.clone()));
        let (conversation_id, usage) = (self.conversation_id, turn.usage);
        let task = tokio::spawn(async move {
            #[cfg(test)]
            test_hooks::during_record(conversation_id).await;
            let wrote = record.await?;
            // Checked again: a caller dropped since the write leaves the
            // indicator to the next turn's call.
            if let Some(context_window) = context_window
                && wrote
                && current.load(Ordering::SeqCst)
            {
                crate::events::publish(
                    conversation_id,
                    crate::events::ConversationEvent::ContextUsageUpdate { usage, context_window },
                );
            }
            Ok::<_, sqlx::Error>(())
        });
        task.await.map_err(ServerFnError::new)?.map_err(ServerFnError::new)
    }
}

/// Marks a finished call's record no longer current when `finish`'s
/// future ends or is dropped.
#[cfg(feature = "server")]
struct NotCurrentOnDrop(Arc<AtomicBool>);

#[cfg(feature = "server")]
impl Drop for NotCurrentOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(feature = "server")]
impl Drop for CallRecorder {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(usage) = self.meter.known() else {
            return;
        };
        let conversation_id = self.conversation_id;
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(conversation_id, "no runtime to record an unfinished model call on");
            return;
        };
        let record = self.record(usage, None);
        runtime.spawn(async move {
            if let Err(e) = record.await {
                tracing::warn!(conversation_id, error = %e, "couldn't record an unfinished model call");
            }
        });
    }
}
