//! A turn: the loop that calls the model, runs the tools it asks for and
//! saves each step, ended early by a stop.

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

/// Bound on how many tool-use turns one `run_turn` call will chase before
/// giving up. Raised from the original placeholder of 5 (fine for `add`/
/// `count`, hit almost immediately by a real multi-step coding session using
/// the sandbox terminal tools) — still not a load-bearing safety limit, just
/// a backstop against looping forever. Exceeding it ends the turn with an
/// error rather than looping forever.
#[cfg(feature = "server")]
pub(super) const MAX_TURNS: usize = 10_000;

/// How long a turn waits for a repo that's still cloning; see `run_turn_bounded`.
#[cfg(feature = "server")]
pub(super) const CLONE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// The smallest reply budget a turn asks for (SME-111, until then every
/// turn's fixed `max_tokens`): the auto-compaction trigger reserves at
/// least this much off the context window before deciding a request is
/// too big, so a turn that doesn't compact has room for it. The budget
/// itself grows with the room left (`reply_budget`).
#[cfg(feature = "server")]
pub(super) const MIN_REPLY_TOKENS: u32 = 16_384;

/// Runs one full tool-use round trip for `conversation_id`: persists
/// `new_message`, then loops calling the real Anthropic API — executing any
/// tool the model asks for and persisting its result — until the model
/// produces a non-`tool_use` turn or `MAX_TURNS` is exceeded. Returns every
/// message persisted along the way, in order, starting with `new_message`
/// itself. Its reply streams to every tab through the conversation's
/// events (`relay_reply_delta`).
/// Returns a boxed, type-erased future rather than using plain `async fn`
/// sugar: `run_turn` calls `anthropic::tools::execute`, and a tool's
/// spawned work can wake the model, calling back into `run_turn` (a
/// finished clone, a command's completion) — that recursion defeats rustc's
/// `Send`-auto-trait inference for plain `async fn`s ("cannot satisfy `impl
/// Future: Send`" with no useful location). Type-erasing one edge of the
/// cycle here breaks it.
#[cfg(feature = "server")]
pub(crate) fn run_turn<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: anthropic::AnthropicMessage,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnResult> + Send + 'a>>
{
    run_turn_bounded(
        pool,
        conversation_id,
        Some(new_message),
        MAX_TURNS,
        false,
        false,
    )
}

/// How a turn that didn't finish ended: the user stopped it, or it
/// failed.
#[derive(Debug)]
pub(crate) enum TurnFailure {
    Stopped,
    Failed(ServerFnError),
}

impl TurnFailure {
    /// The text a tab is shown (`TurnError`): `TURN_STOPPED` for a stop,
    /// which the page shows as "Stopped.".
    pub(crate) fn message(&self) -> String {
        match self {
            TurnFailure::Stopped => TURN_STOPPED.to_string(),
            TurnFailure::Failed(e) => chat_error_text(e),
        }
    }
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnFailure::Stopped => f.write_str(TURN_STOPPED),
            TurnFailure::Failed(e) => e.fmt(f),
        }
    }
}

impl From<ServerFnError> for TurnFailure {
    fn from(e: ServerFnError) -> Self {
        TurnFailure::Failed(e)
    }
}

/// A turn's messages, saved in order, or how it ended early.
pub(crate) type TurnResult = Result<Vec<Message>, TurnFailure>;

/// Adds a message the turn just saved to `persisted` and tells every tab
/// watching the conversation, right away: the user's own message first,
/// then each step of a multi-step reply as it happens, rather than one
/// batch when the turn ends.
#[cfg(feature = "server")]
pub(super) fn record_saved(conversation_id: i64, persisted: &mut Vec<Message>, saved: Message) {
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::MessagesAppended {
            messages: vec![saved.clone()],
        },
    );
    persisted.push(saved);
}

/// A turn, ended early if the user stops it (see `TurnStops`). Stopping
/// drops `run_turn_body` wherever it's waiting (the model's stream, a
/// tool call, a compaction), which releases the turn lock. What it leaves
/// behind: a streaming reply isn't saved, and tool calls without results
/// get answered on the next request (`answer_unfinished_tool_calls`).
/// `from_user` says `new_message` is the user's own (`start_turn`): only
/// that dismisses a question the conversation waits on (SME-34).
#[cfg(feature = "server")]
pub(super) fn run_turn_bounded<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: Option<anthropic::AnthropicMessage>,
    max_turns: usize,
    keep_error: bool,
    from_user: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnResult> + Send + 'a>>
{
    Box::pin(async move {
        // Before the stop receiver, so it's dropped after it: the last turn
        // ending frees the stop counter once nothing listens to it.
        let _in_flight = TurnInFlight::start(conversation_id);
        let mut stop = stop_receiver(conversation_id);
        // Any new turn replaces the last failure (SME-51 code review 1).
        let generation = new_turn_generation(conversation_id);
        let result = run_turn_stoppable(pool, conversation_id, new_message, max_turns, from_user, &mut stop).await;
        // Kept for a tab that reconnects, unless a newer turn has started
        // since this one did: its outcome is the one to show (SME-91).
        if keep_error && let Err(TurnFailure::Failed(e)) = &result {
            keep_turn_error(conversation_id, generation, chat_error_text(e));
        }
        result
    })
}

/// `run_turn_bounded`'s turn: waits for the turn lock, saves the turn's
/// message, and runs it, ending early on a stop (`stop`).
#[cfg(feature = "server")]
pub(super) fn run_turn_stoppable<'a: 'b, 'b>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: Option<anthropic::AnthropicMessage>,
    max_turns: usize,
    from_user: bool,
    stop: &'b mut tokio::sync::watch::Receiver<u64>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnResult> + Send + 'b>>
{
    Box::pin(async move {
        let lock = conversation_lock(conversation_id);
        // Queued behind another turn until the lock is free. Stopped while
        // queued: keep the message anyway (a finished command's notice,
        // say), without running a turn for it; the conversation is paused,
        // so the model sees it next time the user writes (SME-40 F4).
        let guard = tokio::select! {
            guard = lock.clone().lock_owned() => guard,
            _ = stop.changed() => {
                let stopped_by = *stop.borrow_and_update();
                let _turn = lock.lock().await;
                if let Some(message) = &new_message {
                    match db::create_message(pool, conversation_id, &message.role, &message.content).await {
                        Ok(saved) => record_saved(conversation_id, &mut Vec::new(), saved),
                        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't keep a stopped turn's message"),
                    }
                }
                end_stopped_turn(pool, conversation_id, stopped_by).await;
                return Err(TurnFailure::Stopped);
            }
        };
        if !db::conversation_exists(pool, conversation_id)
            .await
            .map_err(ServerFnError::new)?
        {
            return Err(ServerFnError::new("conversation not found").into());
        }
        // Saved holding the lock and outside the stop's `select!`, so a
        // Stop can't land between the INSERT and the turn knowing it's
        // saved, which made the stop path save it again (SME-91).
        // A question the conversation waits on is taken here, under the
        // lock, together with saving the message (SME-34): an answer from
        // the card goes first in this turn's message whoever's turn it is,
        // and the user's own message dismisses a question still waiting.
        let mut persisted = Vec::new();
        let new_content = match save_turn_message(pool, conversation_id, new_message, from_user).await? {
            Some((saved, content)) => {
                #[cfg(test)]
                test_hooks::after_message_saved(conversation_id).await;
                record_saved(conversation_id, &mut persisted, saved);
                Some(content)
            }
            None => None,
        };
        tokio::select! {
            result = run_turn_body(pool, conversation_id, guard, persisted, new_content, max_turns) => return result.map_err(TurnFailure::Failed),
            _ = stop.changed() => {}
        }
        // Stopped mid-turn: the body, and its hold on the lock, is gone.
        let stopped_by = *stop.borrow_and_update();
        let _turn = lock.lock().await;
        end_stopped_turn(pool, conversation_id, stopped_by).await;
        Err(TurnFailure::Stopped)
    })
}

/// Adds `delta` to `conversation_id`'s reply so far and publishes it with
/// its offset. Both happen under the lock `get_reply_in_progress` reads
/// through, so a tab's fetched text and the offsets it then receives
/// agree (SME-51 B3).
#[cfg(feature = "server")]
pub(super) fn relay_reply_delta(conversation_id: i64, delta: &str) {
    append_reply(conversation_id, delta, |offset| {
        crate::events::publish(
            conversation_id,
            crate::events::ConversationEvent::ReplyDelta {
                text: delta.to_string(),
                offset,
            },
        );
    });
}

/// What a stopped turn leaves: no reply in progress, on the server or in
/// any tab, even while another turn is still in flight (the last turn
/// ending clears it too, but a queued one may not end soon; SME-91), and
/// stop `stopped_by` noted (`record_stop`). Call holding the turn lock:
/// replies only stream under it, so this can't clear another turn's.
#[cfg(feature = "server")]
pub(super) async fn end_stopped_turn(pool: &PgPool, conversation_id: i64, stopped_by: u64) {
    clear_reply_in_progress(conversation_id);
    crate::events::publish(conversation_id, crate::events::ConversationEvent::ReplyReset {});
    record_stop(pool, conversation_id, stopped_by).await;
}

/// Notes in the conversation that the user stopped it (`STOP_NOTICE`),
/// once however many turns the stop (`stopped_by`, the stop counter's
/// value) ended: the turns it ended each come here, and a queued one's
/// own message can land after the first note (SME-91). Also not when the
/// last message already is the note. Call holding the turn lock.
#[cfg(feature = "server")]
pub(super) async fn record_stop(pool: &PgPool, conversation_id: i64, stopped_by: u64) {
    if !note_stop(conversation_id, stopped_by) {
        return;
    }
    let already = db::list_messages(pool, conversation_id)
        .await
        .ok()
        .and_then(|messages| messages.last().map(|m| m.content.contains(STOP_NOTICE)))
        .unwrap_or(false);
    if already {
        return;
    }
    let notice = [anthropic::ContentBlock::Text { text: STOP_NOTICE.to_string() }];
    match db::create_message(pool, conversation_id, "user", &notice).await {
        Ok(saved) => record_saved(conversation_id, &mut Vec::new(), saved),
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't note a stop"),
    }
}

/// What `send_message` does: checks the conversation exists, then runs
/// the user's turn in the background and returns without waiting for it.
/// The turn's messages, reply text and end reach every tab on the
/// conversation's event stream (`MessagesAppended`, `ReplyDelta`,
/// `TurnState`), and a failure or stop as `TurnError`. So a sending tab
/// holds no connection of its own while the reply streams.
#[cfg(feature = "server")]
pub(crate) async fn start_turn(pool: PgPool, id: i64, content: String) -> ServerFnResult<()> {
    if !db::conversation_exists(&pool, id)
        .await
        .map_err(ServerFnError::new)?
    {
        return Err(ServerFnError::new("conversation not found"));
    }
    // The user writing again ends any pause from an earlier stop.
    resume_turns(id);
    // At once, not only when the turn starts: it may queue behind another,
    // and a turn already running mustn't keep its error over this one.
    new_turn_generation(id);
    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text: content }],
    };
    tokio::spawn(async move {
        // Its failure is kept for a reconnecting tab (`keep_error`).
        if let Err(e) = run_turn_bounded(&pool, id, Some(new_message), MAX_TURNS, true, true).await {
            let message = e.message();
            crate::events::publish(id, crate::events::ConversationEvent::TurnError { message });
        }
    });
    Ok(())
}

/// The turn itself, holding the turn lock (`turn_lock`): `persisted` starts
/// with the turn's own message, already saved by `run_turn_bounded`, and
/// `new_content` is that message's content (`None` for a wake).
#[cfg(feature = "server")]
pub(super) fn run_turn_body<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    turn_lock: tokio::sync::OwnedMutexGuard<()>,
    mut persisted: Vec<Message>,
    new_content: Option<Vec<anthropic::ContentBlock>>,
    max_turns: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'a>>
{
    Box::pin(async move {
        // Moved in, so the lock is held for as long as the turn runs.
        let _turn = turn_lock;
        // Tracks what's been persisted since the last *real* Anthropic
        // response — the compaction trigger's cheap size estimate (see
        // `should_compact`) is computed over exactly this, added to that
        // last response's own real `usage`. Reset to empty every time a
        // real response lands (below); grows with the new user message,
        // any drained notifications, and (on a later loop iteration) the
        // previous turn's tool-result batch.
        let mut pending_new_content: Vec<anthropic::ContentBlock> = Vec::new();
        let mut last_known_usage = db::get_conversation_usage(pool, conversation_id)
            .await
            .map_err(ServerFnError::new)?;
        let is_wake = new_content.is_none();
        pending_new_content.extend(new_content.into_iter().flatten());

        // The model this turn runs on, read the first time it's needed
        // (after the new message and any pending notices are saved, so
        // they survive a turn that can't run) and then kept: a model or
        // provider changed mid-turn applies from the next turn (SME-72).
        let mut resolved_model: Option<crate::providers::TurnModel> = None;

        // A repo the user just attached may still be cloning; its AGENTS.md
        // belongs in this turn's system prompt (SME-32). Bounded: past it,
        // the turn goes ahead and the prompt says the repo is still cloning.
        if !crate::git::wait_for_clones(pool, conversation_id, CLONE_WAIT).await {
            tracing::info!(conversation_id, "starting a turn while a repo is still cloning");
        }

        for _ in 0..max_turns {
            // Checked at the top of every loop iteration, not just once per
            // `run_turn` call — this is what gives same-turn visibility: if
            // a command finishes partway through a turn's tool-calling
            // loop, the very next iteration already sees the notification,
            // without waiting for a fresh user message. See SME-9's
            // "What" and "How" (the completion-notification design).
            drain_unnotified_terminal_commands(
                pool,
                conversation_id,
                &mut pending_new_content,
                &mut persisted,
            )
            .await?;

            // The model's `ask_user` (SME-34): an answer recorded since
            // this turn saved its own message is taken now, so the model
            // never sees the call unanswered; a question still waiting
            // means this turn's notice is saved and the model hears it with
            // the answer. No row left means none can be answered until this
            // turn asks again (code review 1).
            if take_answer_or_wait(pool, conversation_id, &mut pending_new_content, &mut persisted).await? {
                return Ok(persisted);
            }

            // Nothing to do: `new_message` was `None` (a pure "check for a
            // backlog" wake-up, see `wake_conversation`) and the drain
            // above found nothing pending either — e.g. another concurrent
            // wake, or an unrelated live message, already handled it. No
            // message to send and no reason to call the model, so this
            // returns before ever building a request. Only possible on the
            // very first iteration: every later one already has a real
            // tool_use turn's results to send regardless.
            if is_wake && persisted.is_empty() {
                return Ok(persisted);
            }

            let turn_model = match resolved_model.take() {
                Some(model) => model,
                None => crate::providers::resolve_turn_model(pool, conversation_id)
                    .await
                    .map_err(ServerFnError::new)?,
            };
            let turn_model = &*resolved_model.insert(turn_model);

            // Proactive, not reactive: checked *before* building the
            // request that would be too big, using the last real
            // response's own usage plus a cheap estimate of what's new
            // since then — see `should_compact`/`estimate_tokens`. A
            // failure here fails the whole turn loudly rather than risking
            // the oversized request compaction exists to prevent — see
            // SME-18's "Resolved" decisions.
            if should_compact(
                last_known_usage.as_ref(),
                estimate_tokens(&pending_new_content),
                turn_model.context_window,
            ) {
                compact_conversation(pool, conversation_id, turn_model).await?;
                // The last usage measured the history just summarized: the
                // reply budget below estimates the new one instead.
                last_known_usage = None;
            }

            let history = history_for_request(
                db::list_messages(pool, conversation_id)
                    .await
                    .map_err(ServerFnError::new)?,
                turn_model.thinking_stripped_through,
            )
            .map_err(ServerFnError::new)?;

            let mut request = anthropic::CreateMessageRequest {
                model: turn_model.model.clone(),
                // Set below, once the request's size is known.
                max_tokens: MIN_REPLY_TOKENS,
                system: Some(system_prompt(
                    &prompt_environment(pool, conversation_id, &turn_model.model).await,
                )),
                messages: history,
                stream: true,
                tools: anthropic::tools::tool_definitions(pool).await,
                // Set below, with the reply budget it's a share of.
                thinking: None,
                prompt_caching: turn_model.prompt_caching,
                output_config: turn_model.effort.map(|effort| anthropic::OutputConfig { effort }),
                chat_template_kwargs: turn_model.chat_template_kwargs(turn_model.thinking),
            };

            // As long as the conversation has room for, computed per
            // request: each tool round's from its own usage (SME-111).
            // Thinking shares it with the reply.
            let projected = projected_input(last_known_usage.as_ref(), estimate_tokens(&pending_new_content))
                .unwrap_or_else(|| estimate_request_tokens(&request));
            request.max_tokens = reply_budget(turn_model.context_window, projected, turn_model.output_cap);
            request.thinking = turn_model.thinking_config(request.max_tokens);

            // Every tab watching streams the reply: the text so far is kept
            // for a tab that connects mid-reply, and each delta published.
            let mut relay = |delta: &str| relay_reply_delta(conversation_id, delta);

            // See `is_ollama_thinking_tool_call_corruption` — not always
            // caused by thinking specifically (a local model can just
            // flub a tool call's JSON on its own, e.g. writing a bare `?`
            // for a value it wasn't sure about), so the mitigation is two
            // parts: drop `thinking` once (a plausible contributing
            // factor, and free to try), then fall back to plain retries —
            // regenerating is often enough on its own since sampling
            // varies run to run. Bounded so a model that's reliably bad at
            // one particular call can't loop forever.
            let mut turn = None;
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
                    &request,
                    &mut relay,
                )
                .await
                {
                    Ok(t) => {
                        turn = Some(t);
                        break;
                    }
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
            let turn = turn.ok_or_else(|| ServerFnError::new(last_err))?;

            let saved = db::create_message(pool, conversation_id, "assistant", &turn.content)
                .await
                .map_err(ServerFnError::new)?;
            // Saved (and about to be published as a message), so no longer
            // "in progress".
            clear_reply_in_progress(conversation_id);
            record_saved(conversation_id, &mut persisted, saved);

            // Real usage from this call is the ground truth for "how much
            // context is actually being used" — persisted so
            // `get_context_usage` survives a reload, published live so the
            // indicator updates without one, and tracked here as the new
            // baseline the *next* compaction check starts from (everything
            // up through this response is now accounted for, so
            // `pending_new_content` resets — only what's persisted after
            // this point is "new" again). See
            // SME-18.
            // Also appended to the conversation's call history, in the
            // same transaction, for its cost (SME-106).
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
            last_known_usage = Some(turn.usage);
            pending_new_content.clear();

            if turn.stop_reason != "tool_use" {
                return Ok(persisted);
            }

            let mut result_blocks = Vec::new();
            // The model's `ask_user` (SME-34): one per reply; it gets no
            // result now, and the turn ends waiting on it. Recorded before
            // the reply's other tools run, so a Stop or restart while they
            // run leaves the question on its card (code review 2).
            let mut asked = false;
            for block in &turn.content {
                if let anthropic::ContentBlock::ToolUse { id, name, input } = block
                    && name == crate::questions::ASK_USER
                {
                    let refused = match crate::questions::parse_questions(input) {
                        Ok(_) if asked => {
                            "ask one set of questions at a time: put them all in one ask_user call".to_string()
                        }
                        Ok(questions) => {
                            ask(pool, conversation_id, id, questions).await?;
                            asked = true;
                            continue;
                        }
                        Err(e) => e,
                    };
                    result_blocks.push(anthropic::ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: refused,
                        is_error: Some(true),
                    });
                }
            }
            for block in &turn.content {
                if let anthropic::ContentBlock::ToolUse { id, name, input } = block
                    && name != crate::questions::ASK_USER
                {
                    let result =
                        anthropic::tools::execute(pool, conversation_id, id, name, input).await;
                    let (content, is_error) = match result {
                        Ok(output) => (output, None),
                        Err(message) => (message, Some(true)),
                    };
                    result_blocks.push(anthropic::ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content,
                        is_error,
                    });
                }
            }

            if asked {
                if !result_blocks.is_empty() {
                    let saved = db::create_message(pool, conversation_id, "user", &result_blocks)
                        .await
                        .map_err(ServerFnError::new)?;
                    record_saved(conversation_id, &mut persisted, saved);
                }
                return Ok(persisted);
            }

            let saved = db::create_message(pool, conversation_id, "user", &result_blocks)
                .await
                .map_err(ServerFnError::new)?;
            pending_new_content = result_blocks;
            record_saved(conversation_id, &mut persisted, saved);
        }

        Err(ServerFnError::new(format!(
            "tool-use loop exceeded {max_turns} turns without reaching a final reply"
        )))
    })
}
