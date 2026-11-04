//! A turn's lifecycle, from admission to end: starting the user's turn,
//! the bounded loop `run_turn_body` drives, the reply deltas it streams,
//! and what a stop leaves behind. `run.rs` holds the body's steps.

use super::*;

/// Runs one full tool-use round trip for `conversation_id`: persists
/// `new_message`, then loops calling the real Anthropic API — executing any
/// tool the model asks for and persisting its result — until the model
/// produces a non-`tool_use` turn or `MAX_TURNS` is exceeded. Returns every
/// message persisted along the way, in order, starting with `new_message`
/// itself. Its reply streams to every tab through the conversation's
/// events (`relay_reply_delta`).
/// Returns a boxed, type-erased future rather than using plain `async fn`
/// sugar, as `run_turn_bounded` does, which it wraps: the turn loop calls
/// `anthropic::tools::execute`, and a tool's spawned work can wake the
/// model, calling back into `run_turn_bounded` (a finished clone, a
/// command's completion) — that recursion defeats rustc's
/// `Send`-auto-trait inference for plain `async fn`s ("cannot satisfy `impl
/// Future: Send`" with no useful location). Type-erasing one edge of the
/// cycle here breaks it.
///
/// The tests' way in: it counts the turn when called, before the future
/// is polled, whatever the pause (`TurnInFlight::start`). Production
/// turns are admitted by their callers (SME-216): the user's send or
/// answer ends a pause, and a notice respects it.
#[cfg(all(feature = "server", test))]
pub(crate) fn run_turn<'a>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: anthropic::AnthropicMessage,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnResult> + Send + 'a>>
{
    run_turn_bounded(
        pool,
        TurnInFlight::start(conversation_id),
        Some(new_message),
        MAX_TURNS,
        None,
        false,
    )
}

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
    // Every saved message bumps the conversation's `updated_at`, and the first
    // one gives it its title, so the sidebar's list order and titles change
    // with it (SME-269). A tab watching this conversation already refetches on
    // `MessagesAppended`; this is for one holding no conversation stream.
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    persisted.push(saved);
}

/// A turn, ended early if the user stops it (see `TurnStops`). Stopping
/// drops `run_turn_body` wherever it's waiting (the model's stream, a
/// tool call, a compaction), which releases the turn lock. What it leaves
/// behind: a streaming reply isn't saved, and tool calls without results
/// get answered on the next request (`answer_unfinished_tool_calls`).
/// `from_user` says `new_message` is the user's own (`start_turn`): only
/// that dismisses a question the conversation waits on (SME-34), whether
/// the turn runs or a Stop catches it queued (SME-170). `keep_error` keeps
/// the turn's failure for a tab that reconnects; it's the generation the
/// caller took when the user acted (sending, answering), which stands in
/// for the turn's own if it fails before it takes one (SME-179 review 2).
/// `in_flight` is the turn, already counted by its caller (SME-216), so a
/// Stop sent once the caller has returned ends it.
#[cfg(feature = "server")]
pub(super) fn run_turn_bounded<'a>(
    pool: &'a PgPool,
    mut in_flight: TurnInFlight,
    new_message: Option<anthropic::AnthropicMessage>,
    max_turns: usize,
    keep_error: Option<u64>,
    from_user: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnResult> + Send + 'a>>
{
    use tracing::field::Empty;
    let conversation_id = in_flight.conversation_id();
    Box::pin(async move {
        // The turn's span (SME-137), made when the turn starts: its model
        // calls and tool calls are its children. The model is recorded once
        // the turn resolves it.
        let span = tracing::info_span!(
            "turn",
            conversation_id,
            model = Empty,
            provider_id = Empty,
            turn.steps = Empty,
            turn.stopped = Empty,
            smelt.slot_wait_ms = Empty,
            otel.status_code = Empty,
            otel.status_description = Empty,
        );
        crate::telemetry::in_span(span.clone(), async move {
            #[cfg(test)]
            test_hooks::after_turn_counted(conversation_id).await;
            // A send, an answer or a delivered notice brings the model
            // something new: its end is pod activity (SME-78). Kept in this
            // turn, not the conversation's shared state, which a turn ahead
            // in the queue would take (review 1, B1). A wake has neither, and
            // its drains decide, under the lock (`run_turn_body`).
            let brought_something = new_message.is_some() || keep_error.is_some();
            // The turn's generation, once it holds the lock and has something
            // to tell the model (`run_turn_stoppable`).
            let mut generation = None;
            let place = in_flight.take_place();
            let result = run_turn_stoppable(
                pool,
                conversation_id,
                new_message,
                max_turns,
                from_user,
                // Only the user's sends and answers keep their error.
                keep_error.is_some(),
                place,
                in_flight.stop(),
                &mut generation,
            )
            .await;
            // A stop is the user's choice, not a failure.
            match &result {
                Ok(_) => {}
                Err(TurnFailure::Stopped) => {
                    span.record("turn.stopped", true);
                }
                Err(TurnFailure::Failed(e)) => crate::telemetry::mark_error(&span, &e.to_string()),
                Err(TurnFailure::Noted { reason }) => crate::telemetry::mark_error(&span, reason),
            }
            // Its own words, for a parent's report below.
            let failed_with = match &result {
                Err(TurnFailure::Failed(e)) => Some(chat_error_text(e)),
                Err(TurnFailure::Noted { reason }) => Some(reason.clone()),
                _ => None,
            };
            // Kept for a tab that reconnects, unless a newer turn has started
            // since this one did: its outcome is the one to show (SME-91). A
            // failure the conversation noted keeps only that it was (SME-123
            // DR3): the page shows its notice.
            if let Some(generation) = generation.or(keep_error)
                && keep_error.is_some()
            {
                match &result {
                    Err(TurnFailure::Failed(e)) => keep_turn_error(conversation_id, generation, chat_error_text(e)),
                    Err(TurnFailure::Noted { .. }) => keep_turn_error(conversation_id, generation, TURN_FAILED_NOTED.to_string()),
                    _ => {}
                }
            }
            // SME-71: what this turn's end owes the user, before
            // `drop(in_flight)` takes it (the order the plan fixes). A
            // Stop clears the spell's debt instead: the user is there. An
            // Ok end notes nothing by itself (SME-377): a question was
            // noted at `ask`, the model's `notify_user` at its call, and a
            // plain reply — a model waking on its command's end, say —
            // owes the user nothing it didn't say so.
            match &result {
                Err(TurnFailure::Failed(_) | TurnFailure::Noted { .. }) => {
                    note_attention(conversation_id, crate::events::Attention::Failed)
                }
                Err(TurnFailure::Stopped) => clear_attention(conversation_id),
                Ok(_) => {}
            }
            // A child's failure, cut-off or refusal reaches the user
            // through its parent's report, not the desktop (SME-386 N6).
            // A lookup that fails counts it as a root: it notifies.
            if matches!(db::conversation_parent(pool, conversation_id).await, Ok(Some(_))) {
                forgo_child_outcome(conversation_id);
            }
            // A child agent whose last turn ended without its report tells
            // its parent (SME-236), checked once this turn no longer counts.
            // Not after a stop, nor a wake that found nothing to do.
            // At its end, so a long turn counts from when it finished (SME-78).
            if brought_something {
                crate::sandbox::note_activity(conversation_id);
            }
            drop(in_flight);
            let ended = match &result {
                Ok(saved) if saved.is_empty() => None,
                Ok(_) => Some(None),
                Err(TurnFailure::Failed(_) | TurnFailure::Noted { .. }) => Some(failed_with),
                Err(TurnFailure::Stopped) => None,
            };
            if let Some(failure) = ended
                && !turn_running(conversation_id)
            {
                let pool = pool.clone();
                tokio::spawn(tracing::Instrument::in_current_span(async move {
                    crate::agents::after_last_turn(&pool, conversation_id, failure).await;
                }));
            }
            result
        })
        .await
    })
}

/// `run_turn_bounded`'s turn: waits for the turn lock, in its place in
/// the queue (`place`, taken at its admission; a test's turn joins the
/// queue here), saves the turn's message, and runs it, ending early on a
/// stop (`stop`).
///
/// A turn with a message of its own takes a new turn generation once it
/// holds the lock (`generation`), clearing the kept error. One without
/// (a wake, or an answer from the card) takes one only once it has
/// something to tell the model: the answer it saves here, or notices its
/// drain saves (`run_turn_steps`). Not when it's counted, as before
/// SME-179 (B30): a wake queued behind the user's failing turn took a
/// newer one first, so that failure was never kept, and a wake with
/// nothing to do cleared the error the user was shown. An answer turn
/// whose answer another turn already took has nothing to do either
/// (review 1).
#[cfg(feature = "server")]
pub(super) fn run_turn_stoppable<'a: 'b, 'b>(
    pool: &'a PgPool,
    conversation_id: i64,
    new_message: Option<anthropic::AnthropicMessage>,
    max_turns: usize,
    from_user: bool,
    users_turn: bool,
    place: Option<TurnPlace>,
    stop: &'b mut tokio::sync::watch::Receiver<u64>,
    generation: &'b mut Option<u64>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnResult> + Send + 'b>>
{
    Box::pin(async move {
        // The place's own lock, not the conversation's current one: after
        // a delete's forget that's a fresh lock this turn doesn't hold.
        let TurnPlace { lock, mut acquire } = place.unwrap_or_else(|| {
            let lock = conversation_lock(conversation_id);
            let acquire: TurnAcquire = Box::pin(lock.clone().lock_owned());
            TurnPlace { lock, acquire }
        });
        // Queued behind another turn until the lock is free. Stopped while
        // queued: keep the message anyway (a finished command's notice,
        // say), without running a turn for it; the conversation is paused,
        // so the model sees it next time the user writes (SME-40 F4). It's
        // saved as a turn that ran would save it (SME-170): the user's own
        // message dismisses a question still waiting, and an answer from
        // the card goes first in it. A stopped wake or answer turn has no
        // message, saves nothing, and leaves an answer for the next turn.
        // A turn stopped while queued keeps its place in the queue, so
        // stopped messages are saved in the order they were admitted (SME-202).
        let guard = tokio::select! {
            guard = &mut acquire => guard,
            _ = stop.changed() => {
                let stopped_by = *stop.borrow_and_update();
                #[cfg(test)]
                test_hooks::stopped_while_queued(conversation_id, &new_message).await;
                let _turn = acquire.await;
                if new_message.is_some() {
                    match save_turn_message(pool, conversation_id, new_message, from_user).await {
                        Ok(Some((saved, _))) => record_saved(conversation_id, &mut Vec::new(), saved),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't keep a stopped turn's message"),
                    }
                }
                end_stopped_turn(pool, conversation_id, stopped_by).await;
                return Err(TurnFailure::Stopped);
            }
        };
        if new_message.is_some() {
            *generation = take_turn_generation(conversation_id, &lock);
        }
        // What the drains mark from here on is this turn's: nothing a turn
        // before it left (a panicked one) carries over (review 1, B2).
        take_activity_owed(conversation_id);
        // The user's turn in an agent that finished while this queued
        // behind its last turn makes it active again, so it can report
        // again (SME-236 review 2). A parent's stop isn't undone.
        if users_turn {
            crate::agents::resume_finished(pool, conversation_id).await;
        }
        // Checked after the generation is taken, both under the lock: a
        // delete landing before the check is seen here, and one landing
        // after it forgets the generation itself (`forget_conversation_lock`),
        // after which this turn takes no generation (`take_turn_generation`), so
        // the turn's state ends idle either way (SME-179 B31).
        if !db::conversation_exists(pool, conversation_id)
            .await
            .map_err(ServerFnError::new)?
        {
            forget_turn_generation(conversation_id);
            return Err(crate::api::chat::conversation_not_found().into());
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
                // An answer turn's answer, saved as its message.
                if generation.is_none() {
                    *generation = take_turn_generation(conversation_id, &lock);
                }
                #[cfg(test)]
                test_hooks::after_message_saved(conversation_id).await;
                record_saved(conversation_id, &mut persisted, saved);
                Some(content)
            }
            None => None,
        };
        tokio::select! {
            result = run_turn_body(pool, conversation_id, guard, lock.clone(), persisted, new_content, max_turns, generation, users_turn) => return result,
            _ = stop.changed() => {}
        }
        // Stopped mid-turn: the body, and its hold on the lock, is gone.
        let stopped_by = *stop.borrow_and_update();
        let _turn = lock.lock().await;
        note_drained_activity(conversation_id);
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
    // SME-357: the note carries `NOTICE_MARK` before its words, like every
    // saved notice, so a user typing them isn't answered with a notice.
    let notice = [anthropic::ContentBlock::Text { text: crate::api::chat::mark_notice(STOP_NOTICE) }];
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
        return Err(crate::api::chat::conversation_not_found());
    }
    // Admitted before this returns, so a Stop sent after it ends this turn
    // (SME-216). The user writing again ends any pause from an earlier
    // stop, and its generation is taken at once, not only when the turn
    // starts: it may queue behind another, and a turn already running
    // mustn't keep its error over this one.
    // A child agent still starting takes no turn (SME-236); checked
    // before the turn is counted.
    crate::agents::admit_user_turn(&pool, id).await.map_err(ServerFnError::new)?;
    #[cfg(test)]
    crate::agents::test_hooks::reach(crate::agents::test_hooks::AFTER_USER_ADMITTED, id).await;
    let (in_flight, generation) = TurnInFlight::start_by_user(id);
    // The pause the admission ended, stored for the next start (SME-238).
    spawn_store_pause(&pool, id);
    // A parent's stop between the admission and the count (SME-236).
    crate::agents::confirm_user_turn(&pool, id).await.map_err(ServerFnError::new)?;
    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        // Never taken for one of smelt's notices (SME-354 B19).
        content: vec![anthropic::ContentBlock::Text { text: crate::api::chat::users_text(&content) }],
    };
    // In the request's span, so the turn's is its child (SME-137).
    tokio::spawn(tracing::Instrument::in_current_span(async move {
        // Its failure is kept for a reconnecting tab (`keep_error`).
        if let Err(e) = run_turn_bounded(&pool, in_flight, Some(new_message), MAX_TURNS, Some(generation), true).await {
            let message = e.message();
            crate::events::publish(id, crate::events::ConversationEvent::TurnError { message });
        }
    }));
    Ok(())
}

/// Notes pod activity for what this turn's drains saved (an agent's message,
/// a finished command's notice), taken while the turn still holds the lock,
/// so a turn queued behind it can't take or clear it (SME-78 review 1).
#[cfg(feature = "server")]
pub(super) fn note_drained_activity(conversation_id: i64) {
    if take_activity_owed(conversation_id) {
        crate::sandbox::note_activity(conversation_id);
    }
}
