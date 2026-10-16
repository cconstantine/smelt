//! ChatPanel's live parts (SME-163): the conversation's event stream, the
//! browsing panel's frame stream, the turn timer, the browser's timezone
//! and the initial load. Each is a hook ChatPanel calls once, in order;
//! they were ChatPanel's own code, moved here whole.

use super::*;

/// Counts the seconds a running turn has been running, for the working
/// line, and goes back to 0 once it ends.
#[cfg(feature = "web")]
pub(super) fn use_turn_timer(state: Store<ConversationState>) {
    let turn_running = state.turn_running();
    let mut turn_elapsed = state.turn_elapsed();
    use_hook(move || {
        spawn(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(1000).await;
                if *turn_running.peek() {
                    *turn_elapsed.write() += 1;
                } else if *turn_elapsed.peek() != 0 {
                    turn_elapsed.set(0);
                }
            }
        });
    });
}

/// Read once per page load, not per message: timestamps are stored as
/// effectively-UTC `NaiveDateTime`s with no timezone of their own, and
/// the browser's offset is the only place that information can come
/// from. A hook, not an effect: it reads nothing reactive, so an effect
/// never reran anyway (SME-58). Web-only: there's no browser `Date`
/// during SSR, and 0 (UTC) is a fine fallback for the pre-hydration
/// render either way.
#[cfg(feature = "web")]
pub(super) fn use_timezone_offset(tz_offset_minutes: Signal<i32>) {
    let mut tz_offset_minutes = tz_offset_minutes;
    use_hook(move || {
        spawn(async move {
            if let Ok(value) = document::eval("return -new Date().getTimezoneOffset();").await
                && let Some(offset) = value.as_i64()
            {
                tz_offset_minutes.set(offset as i32);
            }
        });
    });
}

/// Takes the conversation's messages once they load, and opens it at its
/// latest message.
pub(super) fn use_initial_messages(
    initial_messages: Resource<Option<(i64, Result<Vec<Message>, ServerFnError>)>>,
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    transcript_scroll: StickyBottom,
) {
    let mut messages = state.messages();
    let mut load_error = state.load_error();
    use_effect(move || match initial_messages() {
        // Only the open conversation's load (SME-185 B38); one for a
        // conversation since left is followed by the open one's.
        Some(Some((id, _))) if *selected.peek() != Some(id) => {}
        Some(Some((id, Ok(list)))) => {
            let merged = apply_loaded_messages(&messages.peek(), list, id);
            messages.set(merged);
            load_error.set(None);
            // A freshly loaded conversation should open scrolled to its
            // latest message, regardless of where a previous conversation
            // was left scrolled.
            transcript_scroll.stick();
        }
        Some(Some((_, Err(e)))) => load_error.set(Some(server_error_message(&e))),
        Some(None) => {
            messages.set(Vec::new());
            transcript_scroll.stick();
        }
        None => {}
    });
}

/// Live event subscription: opens once per selected conversation and
/// keeps itself open for as long as that conversation stays selected —
/// independent of, and in addition to, whatever `send_message` calls are
/// in flight. Web-only: SSR has no live browser tab to keep a stream
/// open for, and the server-side executor has no reason to run a loop
/// that never terminates on its own. `event_task` holds the previous
/// subscription's handle so switching conversations cancels it outright
/// (`Task::cancel`) rather than relying on the loop to notice on its own
/// — it might be parked in `events.recv().await` with nothing arriving
/// to wake it back up to check.
#[cfg(feature = "web")]
pub(super) fn use_conversation_events(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    stream_errors: Signal<ChatErrors>,
    conversations_changed: Signal<u64>,
    pods_changed: Signal<u64>,
    turns_changed: Signal<u64>,
    questions_changed: Signal<u64>,
    model_changed: Signal<u64>,
) {
    let mut state = state;
    let mut stream_errors = stream_errors;
    let mut conversations_changed = conversations_changed;
    let mut pods_changed = pods_changed;
    let mut turns_changed = turns_changed;
    let mut questions_changed = questions_changed;
    let mut model_changed = model_changed;
    let mut messages = state.messages();
    let mut load_error = state.load_error();
    let mut streaming_reply = state.streaming_reply();
    let mut turn_running = state.turn_running();
    let mut turn_elapsed = state.turn_elapsed();
    let mut todos = state.todos();
    let mut pending_question = state.pending_question();
    let mut repos = state.repos();
    let mut browsing_session_open = state.browsing_session_open();
    let mut browsing_url = state.browsing_url();
    let mut context_usage = state.context_usage();
    let mut live = state.live();
    let mut event_task: Signal<Option<Task>> = use_signal(|| None);
    use_effect(move || {
        if let Some(task) = event_task.write().take() {
            task.cancel();
        }
        live.set(None);
        let Some(id) = selected() else {
            // With no conversation open, the state is about none, so
            // coming back to the one it held still waits for its reset.
            state.conversation().set(None);
            return;
        };
        // Nothing of the conversation left behind shows in this one:
        // its messages, panels, forms and errors (SME-32 code review 8,
        // SME-51 B11) go in one reset.
        state.set(ConversationState { conversation: Some(id), ..ConversationState::default() });

        let handle = spawn(async move {
            let mut pulls = 0u32;
            loop {
                let subscribed = subscribe_conversation_events(id).await;
                // Deleted (from another tab, say): the server refuses
                // to stream it, so say so instead of retrying for good
                // (SME-91). A refused stream doesn't carry the
                // server's reason, so ask the way a page load does.
                if subscribed.is_err()
                    && let Err(e) = get_messages(id).await
                    && server_error_message(&e) == "conversation not found"
                {
                    // Nothing of it stays under the message, and the
                    // sidebar drops it (B44, SME-122): it kept the
                    // transcript and the row until a reload.
                    state.set(ConversationState { conversation: Some(id), ..ConversationState::default() });
                    stream_errors.write().clear(id);
                    load_error.set(Some(server_error_message(&e)));
                    *conversations_changed.write() += 1;
                    break;
                }
                if let Ok(mut events) = subscribed {
                    // One-shot reconciliation pull: a `broadcast`
                    // channel has no replay, so anything published
                    // before this subscription connected would
                    // otherwise be missed. This runs once per
                    // connection (initial load or reconnect), not on a
                    // timer — not the polling loop this replaces.
                    if let Ok(list) = get_messages(id).await {
                        accept_saved_messages(&mut messages.write(), list);
                    }
                    if let Ok(snapshot) = get_sandbox_state(id).await {
                        change_sandbox_panel(state, |pods, terminals| {
                            merge_sandbox_snapshot(pods, terminals, snapshot)
                        });
                    }
                    if let Ok(snapshot) = get_context_usage(id).await {
                        context_usage.set(Some(snapshot));
                    }
                    if let Ok(snapshot) = get_todos(id).await {
                        todos.set(snapshot);
                    }
                    if let Ok(snapshot) = get_pending_question(id).await {
                        pending_question.set(snapshot);
                    }
                    if let Ok(snapshot) = list_conversation_repos(id).await {
                        repos.set(snapshot);
                    }
                    if let Ok((running, elapsed)) = get_turn_state(id).await {
                        turn_running.set(running);
                        // SME-160: the turn's real age from the
                        // server's clock, so a reload mid-turn shows
                        // it counting on rather than from 0s.
                        turn_elapsed.set(elapsed);
                    }
                    if let Ok(reply) = get_reply_in_progress(id).await {
                        streaming_reply.set(reply);
                    }
                    // A turn that failed before this tab connected
                    // (SME-51 B11), or none since a later one started
                    // (B19, SME-122).
                    match get_turn_error(id).await {
                        Ok(Some(error)) => {
                            stream_errors.write().set_turn_error(id, error);
                        }
                        Ok(None) => {
                            stream_errors.write().clear_turn_error(id);
                        }
                        Err(_) => {}
                    }
                    crate::frontend::check_build_id().await;
                    if let Ok(state) = get_browsing_state(id).await {
                        browsing_session_open.set(state.session_open);
                        browsing_url.set(state.url);
                    }
                    pulls += 1;
                    live.set(Some((id, pulls)));

                    loop {
                        let event = match events.recv().await {
                            Some(Ok(event)) => event,
                            Some(Err(_)) | None => break,
                        };
                        match apply_event(state, event) {
                            EventEffect::None => {}
                            EventEffect::ConversationsChanged => *conversations_changed.write() += 1,
                            EventEffect::PodsChanged => *pods_changed.write() += 1,
                            EventEffect::TurnsChanged => *turns_changed.write() += 1,
                            EventEffect::QuestionsChanged => *questions_changed.write() += 1,
                            EventEffect::ModelChanged => {
                                *model_changed.write() += 1;
                                // Another model can mean another context window: the
                                // meter measures against it.
                                spawn(async move {
                                    if let Ok(snapshot) = get_context_usage(id).await
                                        && selected() == Some(id)
                                    {
                                        context_usage.set(Some(snapshot));
                                    }
                                });
                            }
                            EventEffect::TurnError(message) => {
                                stream_errors.write().set_turn_error(id, message);
                            }
                            EventEffect::TurnStarted => {
                                stream_errors.write().clear_turn_error(id);
                            }
                            EventEffect::StaleBundle => *crate::frontend::STALE_BUNDLE.write() = true,
                        }
                    }
                    live.set(None);
                }
                // Stream ended or failed to open — reconnect after a
                // short fixed delay (a guessed default, like `MAX_TURNS`
                // and `count`'s own clamps elsewhere in this codebase;
                // not meant to be a production backoff policy).
                gloo_timers::future::TimeoutFuture::new(1500).await;
            }
        });
        event_task.set(Some(handle));
    });
}

/// A second, independent live subscription — the frame stream isn't
/// part of `ConversationEvent` (see `events::ConversationEvent`'s own
/// doc comment on why frames get a separate channel), and only needs
/// to run while a session is actually open, not for the conversation's
/// whole lifetime the way the main event subscription does. Reacts to
/// both `selected()` (conversation switch) and `browsing_session_open()`
/// (the model opening/closing a session) — either changing tears down
/// any existing subscription and, if a session is now open, starts a
/// fresh one.
#[cfg(feature = "web")]
pub(super) fn use_browser_frames(selected: Memo<Option<i64>>, state: Store<ConversationState>) {
    let mut browsing_session_open = state.browsing_session_open();
    let mut browsing_frame = state.browsing_frame();
    let mut frame_task: Signal<Option<Task>> = use_signal(|| None);
    use_effect(move || {
        if let Some(task) = frame_task.write().take() {
            task.cancel();
        }
        let Some(id) = selected() else { return };
        // Not on the previous conversation's session: this can run on a
        // switch before the reset does.
        if *state.conversation().read() != Some(id) || !browsing_session_open() {
            return;
        }
        let handle = spawn(async move {
            loop {
                if let Ok(mut frames) = subscribe_browser_frames(id).await {
                    while let Some(Ok(frame)) = frames.recv().await {
                        browsing_frame.set(Some(frame.data));
                    }
                }
                // The stream ended or never opened — a network blip, a
                // server restart, or the session closing. Reconnect,
                // unless the session really is gone (this effect then
                // re-runs and hides the panel).
                gloo_timers::future::TimeoutFuture::new(1500).await;
                if let Ok(state) = get_browsing_state(id).await
                    && !state.session_open
                {
                    browsing_session_open.set(false);
                    return;
                }
            }
        });
        frame_task.set(Some(handle));
    });
}
