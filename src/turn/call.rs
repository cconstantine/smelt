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

/// The request slot a turn holds on its own provider (SME-343): taken
/// before its first model call and held, across every call, retry and
/// tool pass, until the turn ends — stop, failure or finish dropping it.
/// A one-slot server therefore never switches to another conversation
/// between one turn's calls (the ticket's Evidence: every switch
/// re-prefills the prompt), and the turn's own calls are told to send
/// through it (`anthropic::stream::SlotPolicy::Held`) rather than
/// queueing behind the slot they already hold. The one exception is a
/// tool pass while the turn's own agents are live on the same provider
/// (`yield_to_own_agents`, SME-354 B6): they get the slot until the turn's
/// next call.
///
/// `refresh` re-checks the pool at each loop iteration: a **resized**
/// limit keeps this very slot (the pool resized in place; review 2's math
/// takes back the slots it's owed as they free), while a **replaced**
/// pool (a cleared limit, or one set after a clear) is given up for the
/// current one — so a limit saved mid-turn applies at the running turn's
/// next call, and a cleared one lets it go.
#[cfg(feature = "server")]
pub(super) struct TurnSlot {
    slot: Option<crate::providers::limiter::Slot>,
    /// Whether this turn's one wait has landed on the turn span: a span
    /// keeps every value an attribute is given (`anthropic::stream`'s
    /// "Each once"), so a refresh's re-take must not record a second one
    /// (T10).
    recorded: bool,
}

#[cfg(feature = "server")]
impl TurnSlot {
    /// Takes the provider's slot, if it has a limit, recording the wait
    /// — zero included, a limited provider's turn always has one to say
    /// (SME-162 review 2) — on the caller's span: `run_turn_steps` calls
    /// this in the turn's span, and its calls record none of their own.
    pub(super) async fn acquire(turn_model: &crate::providers::TurnModel) -> Self {
        Self::take(turn_model, false).await
    }

    /// `recorded` says this turn's wait is already on the span — a
    /// refresh's re-take of a replaced pool then takes its slot without
    /// recording again; a turn that never had a pool to record behind
    /// (unlimited, now limited) records its first here.
    async fn take(turn_model: &crate::providers::TurnModel, recorded: bool) -> Self {
        let Some((slot, waited)) = turn_model.endpoint.slot().await else {
            return Self { slot: None, recorded };
        };
        if !recorded {
            let waited = i64::try_from(waited.as_millis()).unwrap_or(i64::MAX);
            tracing::Span::current().record("smelt.slot_wait_ms", waited);
        }
        Self { slot, recorded: true }
    }

    /// Re-checks the provider's pool (T6, T7): keep holding the current
    /// pool's slot, or give this turn's slot over to what's now there —
    /// including none, in either direction. A re-take records nothing new:
    /// the turn's one wait is its first acquire's, once on the span (T10);
    /// the paths a refresh walks (resized, cleared, replaced) wait not at
    /// all in the tests, and a replaced pool's re-take rides the same held
    /// rules as the first acquire.
    pub(super) async fn refresh(&mut self, turn_model: &crate::providers::TurnModel) {
        let pool = crate::providers::limiter::current(turn_model.provider_id);
        let keep = match (&self.slot, &pool) {
            (Some(slot), Some(pool)) => slot.from_pool(pool),
            (None, None) => true,
            _ => false,
        };
        if !keep {
            // Assigning after the await: the old slot, if any, is given
            // up once the current pool's is in hand (or confirmed gone).
            *self = Self::take(turn_model, self.recorded).await;
        }
    }

    /// Gives the slot up for the tool pass about to run when an agent under
    /// this conversation is `starting` or `active` on the same provider
    /// (SME-354 B6): otherwise the turn's own agents wait for its whole
    /// turn, where before SME-343 they ran between its calls. The next
    /// iteration's `refresh` takes a slot again before the next call,
    /// behind a child already queued (the pool's semaphore is first come,
    /// first served). With no such agent the slot is kept, so a lone
    /// turn's cache stays warm (SME-343); a failed lookup keeps it too.
    pub(super) async fn yield_to_own_agents(&mut self, pool: &PgPool, conversation_id: i64, provider_id: i64) {
        if self.slot.is_none() {
            return;
        }
        match db::live_agent_under_on_provider(pool, conversation_id, provider_id).await {
            Ok(true) => self.slot = None,
            Ok(false) => {}
            Err(e) => tracing::warn!(conversation_id, "couldn't check for agents to give the slot to: {e}"),
        }
    }

    /// The policy for the turn's model calls: send without acquiring;
    /// `None` when the provider has no limit to hold behind.
    pub(super) fn held(&self) -> anthropic::stream::SlotPolicy<'_> {
        anthropic::stream::SlotPolicy::Held(self.slot.as_ref())
    }
}

/// A model call's failure from a turn: a refusal meaning "this request
/// didn't fit the model's context window" — which the turn knows how to
/// answer (SME-73) — and every other way it fails, text already as it
/// ends the turn.
#[cfg(feature = "server")]
pub(super) enum CallError {
    Overflow(crate::anthropic::overflow::Overflow),
    Failed(ServerFnError),
}

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
    slot: anthropic::stream::SlotPolicy<'_>,
    cli: &mut crate::claude_cli::run::TurnCli,
) -> Result<(anthropic::stream::StreamedTurn, CallRecorder), CallError> {
    let mut relay = |delta: &str| relay_reply_delta(conversation_id, delta);
    if turn_model.kind == crate::providers::ProviderKind::ClaudeCli {
        return call_claude_cli(pool, conversation_id, turn_model, request, &mut relay, cli).await;
    }
    let mut last_err = String::new();
    for attempt in 0..=TOOL_CALL_PARSE_RETRIES {
        // A fresh bubble per model call, and per retry: a retry's
        // text replaces a failed attempt's.
        clear_reply_in_progress(conversation_id);
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::ReplyReset {},
        );
        let recorder = CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Turn, request);
        match stream_for_kind(
            pool,
            conversation_id,
            turn_model,
            request,
            &mut relay,
            recorder.meter(),
            slot,
        )
        .await
        {
            Ok(t) => return Ok((t, recorder)),
            // A request that didn't fit short-circuits the parse-retry
            // loop: resending the same oversized history is only refused
            // again — the turn's recovery is what answers it (SME-73).
            Err(anthropic::stream::ModelError::Overflow(overflow)) => return Err(CallError::Overflow(overflow)),
            Err(e)
                if attempt < TOOL_CALL_PARSE_RETRIES
                    && is_ollama_thinking_tool_call_corruption(e.message()) =>
            {
                request.thinking = None;
                request.chat_template_kwargs = turn_model.chat_template_kwargs(false);
                last_err = e.to_string();
            }
            Err(e) => return Err(CallError::Failed(ServerFnError::new(e))),
        }
    }
    Err(CallError::Failed(ServerFnError::new(last_err)))
}

/// One streamed model call without a turn's CLI run, by provider kind
/// (SME-431): an HTTP provider's request streams as always, and a Claude
/// Code provider's (a tools-less, one-message request: the compaction
/// summary) goes to a one-shot `claude -p` (`claude_cli::once`), in the
/// slot the turn already holds.
#[cfg(feature = "server")]
pub(super) async fn stream_for_kind(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &anthropic::CreateMessageRequest,
    on_delta: &mut (dyn FnMut(&str) + Send),
    meter: &anthropic::stream::UsageMeter,
    slot: anthropic::stream::SlotPolicy<'_>,
) -> Result<anthropic::stream::StreamedTurn, anthropic::stream::ModelError> {
    if turn_model.kind == crate::providers::ProviderKind::ClaudeCli {
        // The summary's subscription reading (SME-486), Ok or Err; a
        // leftover from an earlier call is dropped first.
        crate::claude_cli::usage::discard(conversation_id);
        let done = crate::claude_cli::once::once(conversation_id, turn_model, request, on_delta, meter).await;
        crate::claude_cli::usage::record(pool, conversation_id, turn_model.provider_id).await;
        return done;
    }
    anthropic::stream::stream_anthropic_message_metered(&turn_model.endpoint, request, on_delta, meter, slot).await
}

/// `call_model` on a Claude Code provider (SME-430): one API call of the
/// turn's CLI run, with no parse retries (the CLI talks to Anthropic
/// itself). An API call the CLI made on its own after a cut-off reply
/// (S6) is recorded as a call of its own, from `extra`'s drop. A request
/// that didn't fit is an overflow, which the turn's recovery compacts
/// (SME-73, SME-431).
#[cfg(feature = "server")]
async fn call_claude_cli(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    request: &anthropic::CreateMessageRequest,
    relay: &mut (dyn FnMut(&str) + Send),
    cli: &mut crate::claude_cli::run::TurnCli,
) -> Result<(anthropic::stream::StreamedTurn, CallRecorder), CallError> {
    clear_reply_in_progress(conversation_id);
    crate::events::publish(conversation_id, crate::events::ConversationEvent::ReplyReset {});
    // Only this call's subscription reading is recorded (SME-486 A-L1).
    crate::claude_cli::usage::discard(conversation_id);
    let recorder = CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Turn, request);
    let extra = CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Turn, request);
    let called = crate::claude_cli::run::call(pool, conversation_id, turn_model, request, relay, recorder.meter(), extra.meter(), cli).await;
    drop(extra);
    // The call's subscription reading (SME-486), Ok or Err: a capped
    // call's `rejected` is the one that matters most.
    crate::claude_cli::usage::record(pool, conversation_id, turn_model.provider_id).await;
    called.map(|turn| (turn, recorder)).map_err(|e| match cli.take_overflow() {
        Some(overflow) => CallError::Overflow(overflow),
        None => CallError::Failed(ServerFnError::new(e)),
    })
}

/// Each running turn's Claude Code calls' input, cache read and cache
/// write tokens, by conversation (SME-486 T11): `None` until a call adds
/// some. Every Claude Code call's `CallRecorder` adds what it records (the
/// turn's calls, the CLI's `extra` calls, a compaction's summary, a call
/// refused or stopped), so the turn span counts what `model_call_usage`
/// does (round 1 A-L2). `TurnSteps` opens its turn's entry and closes it.
#[cfg(feature = "server")]
fn turn_tokens() -> std::sync::MutexGuard<'static, std::collections::HashMap<i64, Option<[i64; 3]>>> {
    static TOKENS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i64, Option<[i64; 3]>>>> =
        std::sync::LazyLock::new(Default::default);
    // Every change is one insert, remove or add: nothing is left half-done.
    TOKENS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts counting `conversation_id`'s turn's Claude Code tokens.
#[cfg(feature = "server")]
pub(super) fn open_turn_tokens(conversation_id: i64) {
    turn_tokens().insert(conversation_id, None);
}

/// Stops counting, with the turn's summed tokens if any call added some.
#[cfg(feature = "server")]
pub(super) fn close_turn_tokens(conversation_id: i64) -> Option<[i64; 3]> {
    turn_tokens().remove(&conversation_id).flatten()
}

/// Adds one Claude Code call's `usage` to its turn's count, if one is open.
#[cfg(feature = "server")]
fn add_turn_tokens(conversation_id: i64, usage: &anthropic::TokenUsage) {
    if let Some(sum) = turn_tokens().get_mut(&conversation_id) {
        let [input, read, write] = sum.get_or_insert([0; 3]);
        *input += usage.input_tokens;
        *read += usage.cache_read_input_tokens;
        *write += usage.cache_creation_input_tokens;
    }
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
    estimated_input: i64,
    armed: bool,
    /// A Claude Code call: what it records counts on its turn's span too.
    claude_code: bool,
}

#[cfg(feature = "server")]
impl CallRecorder {
    pub(super) fn new(
        pool: &PgPool,
        conversation_id: i64,
        turn_model: &crate::providers::TurnModel,
        kind: db::ModelCallKind,
        request: &anthropic::CreateMessageRequest,
    ) -> Self {
        Self {
            pool: pool.clone(),
            conversation_id,
            provider_id: turn_model.provider_id,
            model: turn_model.model.clone(),
            price_catalog_provider: turn_model.price_catalog_provider.clone(),
            kind,
            meter: anthropic::stream::UsageMeter::default(),
            estimated_input: i64::try_from(estimate_request_tokens(request)).unwrap_or(i64::MAX),
            armed: true,
            claude_code: turn_model.kind == crate::providers::ProviderKind::ClaudeCli,
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
        if self.claude_code {
            add_turn_tokens(self.conversation_id, &turn.usage);
        }
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

/// An unfinished call's `usage`, with `estimated_input` (the request's
/// size, `estimate_request_tokens`) as its input when `message_start`
/// reported no input at all (code review 1). LiteLLM's always says 0,
/// its real counts coming only in the final `message_delta`, so a call of
/// it stopped mid-stream would otherwise be counted with no input. Any
/// reported count, a cached one included, stands.
#[cfg(feature = "server")]
pub(super) fn with_estimated_input(usage: anthropic::TokenUsage, estimated_input: i64) -> anthropic::TokenUsage {
    let reported_none = usage.input_tokens == 0
        && usage.cache_creation_input_tokens == 0
        && usage.cache_read_input_tokens == 0;
    if reported_none {
        anthropic::TokenUsage { input_tokens: estimated_input, ..usage }
    } else {
        usage
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
        let usage = with_estimated_input(usage, self.estimated_input);
        let conversation_id = self.conversation_id;
        if self.claude_code {
            add_turn_tokens(conversation_id, &usage);
        }
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
