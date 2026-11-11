//! A turn: the loop that calls the model, runs the tools it asks for and
//! saves each step, ended early by a stop.

use super::*;

/// Bound on how many tool-use turns one `run_turn_bounded` call chases before
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

/// The smallest reply budget a turn usually asks for (SME-111, until then
/// every turn's fixed `max_tokens`): the auto-compaction trigger reserves at
/// least this much off the context window before deciding a request is
/// too big, so a turn that doesn't compact has room for it. The budget
/// itself grows with the room left (`reply_budget`). Past the trigger, where
/// compacting can't help, a request gets only the room left, which can be
/// less (`budget_within_room`, SME-386).
#[cfg(feature = "server")]
pub(super) const MIN_REPLY_TOKENS: u32 = 16_384;

/// Whether the conversation has said `reason` as a failure since the model
/// last replied: a wake failing as the last one did has nothing to add
/// (SME-386 review L6), though a notice may have come between (a command's
/// finish is what woke it).
#[cfg(feature = "server")]
async fn already_said(pool: &PgPool, conversation_id: i64, reason: &str) -> bool {
    let Ok(messages) = db::list_messages(pool, conversation_id).await else {
        return false;
    };
    messages.iter().rev().take_while(|m| m.role == "user").any(|m| {
        m.blocks().unwrap_or_default().iter().any(|block| {
            matches!(block, anthropic::ContentBlock::Text { text } if crate::api::chat::parse_failure_notice(text) == Some(reason))
        })
    })
}

/// Saves the failure notice for a turn that failed with `reason`
/// (`api::chat::failure_notice`). Whether it was saved: if not, the page
/// still shows the error itself.
#[cfg(feature = "server")]
async fn note_failure(pool: &PgPool, conversation_id: i64, reason: &str) -> bool {
    let notice = [anthropic::ContentBlock::Text { text: crate::api::chat::failure_notice(reason) }];
    match db::create_message(pool, conversation_id, "user", &notice).await {
        Ok(saved) => {
            record_saved(conversation_id, &mut Vec::new(), saved);
            true
        }
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't note a failed turn");
            false
        }
    }
}

/// Clears `conversation_id`'s answer owed at a turn's end, holding the
/// turn lock (SME-450). A failure only logs: the mark stays, and the next
/// wake or restart answers it once.
#[cfg(feature = "server")]
pub(super) async fn clear_answer_owed(pool: &PgPool, conversation_id: i64) {
    if let Err(e) = db::clear_answer_owed(pool, conversation_id).await {
        tracing::warn!(conversation_id, error = %e, "couldn't clear an answer owed at a turn's end");
    }
}

/// Clears the report-owed mark (SME-455) where the user engages a seat:
/// its send (`start_turn`) and its card answer. Warn-only like the
/// clear above: losing it costs one redundant escalation at the seat's
/// next stuck end, never a lost notice.
#[cfg(feature = "server")]
pub(super) async fn clear_report_owed(pool: &PgPool, conversation_id: i64) {
    if let Err(e) = db::clear_report_owed(pool, conversation_id).await {
        tracing::warn!(conversation_id, error = %e, "couldn't clear a report owed to the parent");
    }
}

/// The compaction loop's notice for the model, if the compaction just run
/// counted `NOTICE_AT` of them (SME-414): saved grey (SME-357) by
/// `save_notice`, which puts it on the next request too, so the model reads
/// it while the loop can still bend (and before the turn ends at `END_AT`).
#[cfg(feature = "server")]
async fn note_thrash(
    pool: &PgPool,
    conversation_id: i64,
    persisted: &mut Vec<Message>,
    pending_new_content: &mut Vec<anthropic::ContentBlock>,
) -> ServerFnResult<()> {
    if let Some(notice) = with_compaction_progress(conversation_id, CompactionProgress::take_thrash_notice) {
        save_notice(pool, conversation_id, crate::api::chat::mark_notice(&notice), persisted, pending_new_content).await?;
    }
    Ok(())
}

/// Records how many model calls a turn made on its span once the turn ends,
/// whichever way it does (a return, an error, a stop dropping it).
struct TurnSteps {
    span: tracing::Span,
    count: i64,
}

impl Drop for TurnSteps {
    fn drop(&mut self) {
        self.span.record("turn.steps", self.count);
    }
}

/// The turn itself, holding the turn lock (`turn_lock`, of mutex `held`)
/// until it ends:
/// `persisted` starts with the turn's own message, already saved by
/// `run_turn_bounded`, and `new_content` is that message's content
/// (`None` for a wake). `generation` is the turn's generation, if it has
/// taken one; a turn that hasn't takes one before it first calls the
/// model. A turn that fails leaves no reply in progress,
/// as a stopped one doesn't (`end_stopped_turn`): cleared, and every tab
/// told, while it still holds the lock, so it can't be another turn's
/// and a turn queued behind it doesn't show it (SME-179 B28).
#[cfg(feature = "server")]
pub(super) fn run_turn_body<'a: 'b, 'b>(
    pool: &'a PgPool,
    conversation_id: i64,
    turn_lock: tokio::sync::OwnedMutexGuard<()>,
    held: Arc<tokio::sync::Mutex<()>>,
    persisted: Vec<Message>,
    new_content: Option<Vec<anthropic::ContentBlock>>,
    max_turns: usize,
    generation: &'b mut Option<u64>,
    users_turn: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = TurnResult> + Send + 'b>>
{
    Box::pin(async move {
        // Moved in, so the lock is held for as long as the turn runs.
        let _turn = turn_lock;
        let result = run_turn_steps(pool, conversation_id, &held, persisted, new_content, max_turns, generation).await;
        // The turn has ended, Ok or Err: what it was owed, it tried to
        // answer, and neither a restart nor the next wake tries again (D7).
        clear_answer_owed(pool, conversation_id).await;
        note_drained_activity(conversation_id);
        let Err(e) = result else {
            return result.map_err(TurnFailure::Failed);
        };
        clear_reply_in_progress(conversation_id);
        crate::events::publish(conversation_id, crate::events::ConversationEvent::ReplyReset {});
        // A user's turn says why it failed in the conversation, saved here,
        // still holding the lock, so it lands before anything a turn queued
        // behind this one saves (SME-123 DR3, review 1 M1). A Stop landing
        // during this one insert drops it with the body: the turn then ends
        // as stopped, with the stop's own notice, which is what the user
        // asked for (review 2 L1, accepted). Any of a child's turns does too,
        // whoever's they are: they run with no tab open, the banner is live
        // only, and the user can open the child and write to it (SME-386
        // F7). A lookup that fails counts it as a root, as elsewhere. And a
        // conversation that doesn't fit its window says so, on a root's wake
        // too: nothing else will tell the user why (SME-386 F10, review L4).
        // The user's own send always gets its answer; another turn failing
        // as the last one did says nothing new (review L6).
        let is_child = matches!(db::conversation_parent(pool, conversation_id).await, Ok(Some(_)));
        let doesnt_fit = with_compaction_progress(conversation_id, |p| p.is_stuck());
        if users_turn || is_child || doesnt_fit {
            let reason = chat_error_text(&e);
            if (!users_turn && already_said(pool, conversation_id, &reason).await)
                || note_failure(pool, conversation_id, &reason).await
            {
                return Err(TurnFailure::Noted { reason });
            }
        }
        Err(TurnFailure::Failed(e))
    })
}

/// `run_turn_body`'s steps: the loop that calls the model, runs the tools
/// it asks for and saves each step.
#[cfg(feature = "server")]
fn run_turn_steps<'a: 'b, 'b>(
    pool: &'a PgPool,
    conversation_id: i64,
    held: &'b Arc<tokio::sync::Mutex<()>>,
    persisted: Vec<Message>,
    new_content: Option<Vec<anthropic::ContentBlock>>,
    max_turns: usize,
    generation: &'b mut Option<u64>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'b>>
{
    Box::pin(async move {
        // A Claude Code turn's CLI run, held, like the slot, to the end
        // (SME-430): dropping it ends the run.
        let mut turn_cli = crate::claude_cli::run::TurnCli::default();
        let result = run_turn_loop(pool, conversation_id, held, persisted, new_content, max_turns, generation, &mut turn_cli).await;
        // A turn that returned, Ok or Err, answers the calls its run still
        // waits on before ending it (SME-444). A Stop drops this future
        // before here, so its calls still read as stopped.
        turn_cli.close();
        result
    })
}

/// `run_turn_steps`' loop, on the turn's CLI run `turn_cli`.
#[cfg(feature = "server")]
fn run_turn_loop<'c>(
    pool: &'c PgPool,
    conversation_id: i64,
    held: &'c Arc<tokio::sync::Mutex<()>>,
    mut persisted: Vec<Message>,
    new_content: Option<Vec<anthropic::ContentBlock>>,
    max_turns: usize,
    generation: &'c mut Option<u64>,
    turn_cli: &'c mut crate::claude_cli::run::TurnCli,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerFnResult<Vec<Message>>> + Send + 'c>>
{
    Box::pin(async move {
        // The turn's own span, `run_turn_bounded`'s: its model and the
        // number of model calls it made, recorded however it ends.
        let mut steps = TurnSteps { span: tracing::Span::current(), count: 0 };
        // Tracks what's been persisted since the last *real* Anthropic
        // response — the compaction trigger's cheap size estimate (see
        // `CompactionProgress::step`) is computed over exactly this, added to that
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
        // The request slot this turn holds on its model's provider once
        // that model is resolved, and holds to the end (SME-343).
        let mut turn_slot: Option<crate::turn::call::TurnSlot> = None;

        // A repo the user just attached may still be cloning; its AGENTS.md
        // belongs in this turn's system prompt (SME-32). Bounded: past it,
        // the turn goes ahead and the prompt says the repo is still cloning.
        if !crate::git::wait_for_clones(pool, conversation_id, CLONE_WAIT).await {
            tracing::info!(conversation_id, "starting a turn while a repo is still cloning");
        }

        let mut streak = TextCallStreak::default();
        // The number this turn's window stops carry as evidence (SME-439
        // round 1 M1): every model call below shares it — a `carries_on`
        // continuation included — so the candidate map can refuse a
        // like-sized restop inside one turn the repeat it would wrongly
        // learn from.
        let window_stop_turn = super::overflow::next_window_stop_turn();
        for _ in 0..max_turns {
            // The model's `ask_user` (SME-34): an answer recorded since
            // this turn saved its own message is taken now, so the model
            // never sees the call unanswered. A question still waiting
            // stops this iteration *before* it consumes anything: a notice
            // saved and marked here could never be re-woken
            // (`wake_has_work` sees consumed as done), so it stays queued
            // and the turn that resolves the question — a card answer, the
            // parent's reply, or the user's dismissive message — tells it
            // (SME-409). No row left means none can be answered until this
            // turn asks again (code review 1).
            if take_answer_or_wait(pool, conversation_id, &mut pending_new_content, &mut persisted).await? {
                return Ok(persisted);
            }
            // Checked at the top of every loop iteration past the question
            // above, not just once per `run_turn_bounded` call — this is
            // what gives same-turn visibility: if a command finishes
            // partway through a turn's tool-calling loop, the very next
            // iteration already sees the notification, without waiting for
            // a fresh user message. See SME-9's "What" and "How" (the
            // completion-notification design).
            drain_unnotified_terminal_commands(
                pool,
                conversation_id,
                &mut pending_new_content,
                &mut persisted,
            )
            .await?;
            // Commands still *running* but left untouched long enough to
            // earn an idle notice (SME-79) — the same every-iteration
            // visibility the finish notices above have.
            drain_idle_command_notices(pool, conversation_id, &mut pending_new_content, &mut persisted)
                .await?;
            // Reports and notices from agents (SME-236), likewise.
            drain_agent_messages(pool, conversation_id, &mut pending_new_content, &mut persisted).await?;

            // Nothing to do: `new_message` was `None` (a pure "check for a
            // backlog" wake-up, see `wake_conversation`) and the drain
            // above found nothing pending either — e.g. another concurrent
            // wake, or an unrelated live message, already handled it. No
            // message to send and no reason to call the model, so this
            // returns before ever building a request. Only possible on the
            // very first iteration: every later one already has a real
            // tool_use turn's results to send regardless.
            // Unless a turn a restart dropped left an answer owed
            // (SME-450): the model answers its history as stored, which
            // ends in what it was owed. Checked under the turn lock, so a
            // second wake for the same answer finds it cleared and stops.
            if is_wake && persisted.is_empty() && !db::answer_owed(pool, conversation_id).await.map_err(ServerFnError::new)? {
                return Ok(persisted);
            }
            // A wake (or an answer turn) with notices to tell is a newer
            // turn, so it replaces the kept error, once, as it first calls
            // the model (SME-179).
            if generation.is_none() {
                *generation = take_turn_generation(conversation_id, &held);
            }

            let model = match resolved_model.take() {
                Some(model) => model,
                None => crate::providers::resolve_turn_model(pool, conversation_id)
                    .await
                    .map_err(ServerFnError::new)?,
            };
            // The turn's own copy, mutable: an overflow refusal's window
            // lands on it so every later size of the turn reads it (SME-73).
            let turn_model = resolved_model.insert(model);
            // Take the provider's request slot before this turn's first
            // model call, and re-check the pool every iteration after
            // that: a limit saved (or cleared) mid-turn applies here, at
            // the turn's next call (SME-343). Held — not re-taken — by
            // every call and compaction below until this turn ends.
            let slot = match &mut turn_slot {
                Some(slot) => slot,
                slot @ None => {
                    #[cfg(test)]
                    test_hooks::before_turn_slot(conversation_id);
                    let acquired = TurnSlot::acquire(turn_model).await;
                    slot.insert(acquired)
                }
            };
            slot.refresh(turn_model).await;
            if steps.count == 0 {
                steps.span.record("model", turn_model.model.as_str());
                steps.span.record("provider_id", turn_model.provider_id);
            }
            steps.count += 1;

            // Proactive, not reactive: checked *before* building the
            // request that would be too big, using the last real
            // response's own usage plus a cheap estimate of what's new
            // since then — see `CompactionProgress::step`/`estimate_tokens`. A
            // failure here fails the whole turn loudly rather than risking
            // the oversized request compaction exists to prevent — see
            // SME-18's "Resolved" decisions.
            let projected = projected_input(last_known_usage.as_ref(), estimate_tokens(&pending_new_content));
            let mut compacted = false;
            match with_compaction_progress(conversation_id, |p| p.step(&turn_model.model, turn_model.context_window, projected)) {
                CompactionStep::Go => {}
                // Compacting again would only repeat what the last ones
                // showed: the turn ends, saying why (SME-386 F10).
                CompactionStep::Stuck(why) => return Err(ServerFnError::new(why)),
                // A Claude Code conversation too (SME-431): its summary is
                // a one-shot CLI call, and its session restarts from it.
                CompactionStep::Compact => {
                    // A proactive compaction: no refusal to name on the
                    // divider (SME-73).
                    compact_conversation(pool, conversation_id, turn_model, slot.held(), None).await?;
                    with_compaction_progress(conversation_id, |p| p.compacted(turn_model.context_window));
                    note_thrash(pool, conversation_id, &mut persisted, &mut pending_new_content).await?;
                    compacted = true;
                    // The last usage measured the history just summarized: the
                    // reply budget below estimates the new one instead.
                    last_known_usage = None;
                }
            }

            let (mut request, budget) = build_request_with_room(
                pool,
                conversation_id,
                turn_model,
                slot.held(),
                &mut last_known_usage,
                &pending_new_content,
                &mut compacted,
                false,
                None,
                Undercount::none(),
            )
            .await?;
            // A floor compaction can run inside the build (SME-414): its
            // notice is saved before the call, riding the next request.
            note_thrash(pool, conversation_id, &mut persisted, &mut pending_new_content).await?;
            // What a compaction would have to free, for a cut-off reply below.
            let mut sized = projected_input(last_known_usage.as_ref(), estimate_tokens(&pending_new_content));
            let offered: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
            // A refusal meaning "this didn't fit" answers itself once:
            // learn the window, shrink, send again (SME-73). Every
            // other failure ends the turn as it always did.
            let called = call_model(pool, conversation_id, turn_model, &mut request, slot.held(), turn_cli).await;
            let (turn, recorder, budget) = match called {
                Ok((turn, recorder)) => {
                    // Ollama truncates an oversized request quietly rather
                    // than refusing it: when this reply measured far below
                    // the request that went, ask the server what window it
                    // really loaded (SME-73). The reply is kept as it
                    // stands; this only fixes the next request — and it
                    // runs before the meter publish below, so the meter
                    // reads the new window with this reply's own usage.
                    super::overflow::learn_ollama_window(pool, conversation_id, turn_model, &request, &turn.usage).await;
                    with_compaction_progress(conversation_id, CompactionProgress::step_taken);
                    (turn, recorder, budget)
                }
                Err(CallError::Overflow(overflow)) => {
                    // Compacting at every step, with nothing a server's silence
                    // on usage could stop (SME-386 review L2).
                    if let Some(why) = with_compaction_progress(conversation_id, |p| p.why_refused_out(&turn_model.model, turn_model.context_window)) {
                        return Err(ServerFnError::new(why));
                    }
                    with_compaction_progress(conversation_id, |p| p.refused(overflow.input_tokens));
                    let recovered = overflow_recovery(
                        pool,
                        conversation_id,
                        turn_model,
                        last_known_usage,
                        &pending_new_content,
                        slot.held(),
                        &request,
                        &overflow,
                    )
                    .await?;
                    if recovered.compacted {
                        sized = None;
                    }
                    (recovered.turn, recovered.recorder, recovered.budget)
                }
                Err(CallError::Failed(e)) => return Err(e),
            };
            // A refusal-answering compaction may have run in the recovery
            // (SME-414): its notice rides the next request as the others' do.
            note_thrash(pool, conversation_id, &mut persisted, &mut pending_new_content).await?;
            // A reply the window stopped, rather than one that ran out of
            // budget, measured the room the server really has — but only
            // against what the server reports, or a repeat of itself,
            // does that measurement teach anything (SME-439). Kept as it
            // stands — a cut-off reply isn't continued — and this runs
            // before the meter is published below, so a save moves the
            // meter too.
            if super::overflow::window_stop(turn_model.kind, &turn.stop_reason, turn.usage.output_tokens, budget.tokens) {
                super::overflow::learn_window_stop(pool, conversation_id, window_stop_turn, turn_model, &turn.usage, &turn.content, &turn.stop_reason).await;
            }
            save_reply(pool, conversation_id, &turn, &mut persisted).await?;
            #[cfg(test)]
            test_hooks::after_reply_saved(conversation_id).await;
            recorder.finish(&turn, Some(turn_model.context_window)).await?;
            // A whole reply that filled more than the learned window just
            // fit through it: that number is too small, and this is where
            // the proof has landed (SME-439 PR 2, Expected 3b). Nothing
            // else about this reply changes, and a failed forget stops
            // nothing — the function swallows its own error.
            crate::providers::learned_window_beaten(pool, turn_model, &turn.usage).await;
            // The new baseline the *next* compaction check starts from:
            // everything up through this response is now accounted for, so
            // `pending_new_content` resets — only what's persisted after
            // this point is "new" again (SME-18).
            with_compaction_progress(conversation_id, |p| p.measured(&turn.usage, turn_model.context_window));
            last_known_usage = Some(turn.usage);
            pending_new_content.clear();
            // The room cut this reply off, where a compaction would free it:
            // that's the cost of going past the usual trigger, not the end of
            // the turn (SME-386 round 2 M1). It's told, compacts, and goes again.
            let cut_off = anthropic::stream::is_cut_off(&turn.stop_reason);
            let carries_on = with_compaction_progress(conversation_id, |p| {
                if !cut_off {
                    p.reply_finished();
                }
                cut_off && budget.limit == crate::api::chat::ReplyLimit::RoomLeft && p.cut_for_room(sized, turn_model.context_window)
            });
            save_stop_notice(pool, conversation_id, turn_model, &turn, &budget, carries_on, &mut persisted).await?;
            if carries_on {
                continue;
            }
            // A cut-off reply ends the turn before any next step could say it
            // doesn't fit, which this reply's measure just showed (round 2 M2).
            if cut_off && let Some(why) = with_compaction_progress(conversation_id, |p| p.why_if_stuck(&turn_model.model, turn_model.context_window)) {
                return Err(ServerFnError::new(why));
            }

            // A call written as text gets a correction, and the model is
            // called again (SME-286).
            let offered: Vec<&str> = offered.iter().map(String::as_str).collect();
            let correction = match after_reply(
                pool,
                conversation_id,
                &turn,
                &offered,
                &mut streak,
                &mut persisted,
                &mut pending_new_content,
            )
            .await?
            {
                NextStep::RunTools(correction) => correction,
                NextStep::CallAgain => continue,
                NextStep::End => return Ok(persisted),
            };

            // The turn's own agents on this provider get the slot while its
            // tools run; the next iteration takes it back (SME-354 B6).
            slot.yield_to_own_agents(pool, conversation_id, turn_model.provider_id).await;
            // Asked the user, or an agent finished (SME-236): the turn ends,
            // with any results saved. A reply here makes at least one call.
            let ran = run_tools(pool, conversation_id, &turn, &offered).await?;
            let (result_blocks, ends) = (ran.results, ran.ends);
            if !result_blocks.is_empty() {
                let saved = db::create_message(pool, conversation_id, "user", &result_blocks)
                    .await
                    .map_err(ServerFnError::new)?;
                record_saved(conversation_id, &mut persisted, saved);
            }
            // Saved: the run's waiting calls are answered with them if the
            // turn returns before its next call (SME-444).
            turn_cli.hold_results(&result_blocks, ran.asked.as_deref());
            pending_new_content = result_blocks;
            // A call the reply wrote as text, told after the real calls'
            // results, even when the turn ends here (SME-286).
            if let Some(correction) = correction {
                save_notice(pool, conversation_id, correction, &mut persisted, &mut pending_new_content).await?;
            }
            // Calls that all named no tool count toward the correction
            // bound, settled after their results are saved (SME-307).
            streak.after_calls(ran.unknown.as_deref()).map_err(ServerFnError::new)?;
            if ends {
                return Ok(persisted);
            }
        }

        Err(ServerFnError::new(format!(
            "tool-use loop exceeded {max_turns} turns without reaching a final reply"
        )))
    })
}
