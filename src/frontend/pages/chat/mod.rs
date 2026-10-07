use std::collections::HashMap;

use dioxus::html::geometry::{PixelsVector2D, WheelDelta};
use dioxus::html::input_data::MouseButton;
#[cfg(feature = "web")]
use dioxus::prelude::dioxus_core::Task;
use dioxus::prelude::*;

use super::server_error_message;
use super::TwoStepButton;
use super::ErrorText;

use crate::anthropic::{ContentBlock, TokenUsage};
// `TodoItem`/`TodoStatus` are used unconditionally (the todo panel itself
// renders in the shared SSR body, not gated behind `web`).
use crate::anthropic::tools::{TodoItem, TodoStatus};
#[cfg(any(feature = "web", test))]
use crate::api::sandbox::SandboxSnapshot;
use crate::api::browsing::{navigate_browser, send_browser_input};
use crate::api::chat::{
    ContextDetailSnapshot, ContextUsageSnapshot, create_conversation,
    delete_conversation, get_context_detail, get_conversations, get_messages, send_message,
};
// Only called from the live event-subscription loops below, which are
// `web`-only (see their own cfg) — a native `server`-only build never
// reaches them.
#[cfg(feature = "web")]
use crate::api::chat::{
    get_context_usage, get_reply_in_progress, get_todos,
    get_turn_error, get_turn_state, subscribe_conversation_events,
};
#[cfg(feature = "web")]
use crate::api::sandbox::get_sandbox_state;
#[cfg(feature = "web")]
use crate::api::browsing::{get_browsing_state, subscribe_browser_frames};
#[cfg(feature = "web")]
use crate::api::git::list_conversation_repos;
use crate::api::git::{attach_repo, decide_repo_trust};
use crate::git::{RepoStatus, RepoSummary};
use crate::browsing::BrowserInputEvent;
// Only referenced by this module's own tests, which build their own
// `SandboxSnapshot`s by hand rather than through `get_sandbox_state`.
#[cfg(test)]
use crate::api::sandbox::{
    SandboxCommandSummary, SandboxOutputLine, SandboxPodSummary, SandboxTerminalSummary,
};
#[cfg(any(feature = "web", test))]
use crate::events::ConversationEvent;
use crate::questions::{PendingQuestion, QuestionAnswer, ASK_USER};
use crate::api::questions::{answer_question, get_waiting_conversations};
#[cfg(feature = "web")]
use crate::api::questions::get_pending_question;
use crate::events::SandboxPreview;
use crate::frontend::Route;
use crate::models::{Conversation, Message};

mod composer;
mod context;
mod model_setup;
mod panels;
mod question_card;
mod repo_attach;
mod sidebar;
mod state;
mod sticky;
mod streaming;
mod transcript;
mod trust_card;

use composer::*;
use context::*;
use model_setup::*;
use panels::*;
use question_card::*;
use repo_attach::*;
use sidebar::*;
use state::*;
use sticky::*;
use streaming::*;
use transcript::*;
use trust_card::*;

#[cfg(test)]
mod tests;

/// The URL is the source of truth for which conversation is selected (so
/// a refresh lands back on the same one) — `selected` reads it straight
/// from the router via `use_memo`, rather than through a prop synced by a
/// `use_effect`. That first approach looked reasonable but silently
/// broke: `use_effect` only re-runs when it reads a tracked reactive
/// value, and a plain `Option<i64>` prop isn't one, so `selected` synced
/// once on mount and then never again — the URL and sidebar highlight
/// kept moving (they read the route/signal directly) but the messages,
/// tasks, and sandbox panel all froze on whatever conversation loaded
/// first. `router.current::<Route>()` performs a genuine tracked signal
/// read, so wrapping it in `use_memo` gives every descendant (including
/// hooks like `use_resource`, which only restarts for a tracked read
/// inside its own closure) a value that actually updates on navigation.
#[component]
pub fn Chat() -> Element {
    let router = router();
    let selected: Memo<Option<i64>> = use_memo(move || match router.current::<Route>() {
        Route::Home {} => None,
        Route::ConversationRoute { id } => Some(id),
        // `Chat` is never actually rendered on these routes (see their own
        // components in `frontend/mod.rs`) — these arms only exist to
        // satisfy exhaustiveness.
        Route::McpServersRoute {} => None,
        Route::McpServerNewRoute {} => None,
        Route::McpServerEditRoute { .. } => None,
        Route::SandboxVolumesRoute {} => None,
        Route::PodsRoute {} => None,
        Route::GitRoute {} => None,
        Route::LanguageServersRoute {} => None,
        Route::LanguageServerNewRoute {} => None,
        Route::LanguageServerEditRoute { .. } => None,
        Route::SandboxVolumeNewRoute {} => None,
        Route::ProvidersRoute {} => None,
        Route::ProviderNewRoute {} => None,
        Route::ProviderEditRoute { .. } => None,
        Route::NotFound { .. } => None,
    });

    // Bumped by the chat panel whenever its conversation gets new
    // messages; the sidebar refetches its list when it changes, so a
    // conversation's title (set by its first message) and its place in the
    // list stay current without a reload.
    let conversations_changed = use_signal(|| 0u64);
    // Bumped by the chat panel whenever a pod is created or goes away in
    // any conversation (`ConversationEvent::PodsChanged`, relayed on the
    // open conversation's own stream rather than a second connection; see
    // that variant). With no conversation open there's no stream, so the
    // sidebar's pod dots only refresh on navigation.
    let pods_changed = use_signal(|| 0u64);
    // Bumped the same way when a turn starts or ends in any conversation
    // (`ConversationEvent::TurnsChanged`), for the sidebar's busy marks.
    let turns_changed = use_signal(|| 0u64);
    // Bumped when a conversation starts or stops waiting on an answer to
    // the model's question (`ConversationEvent::QuestionsChanged`, SME-34).
    let questions_changed = use_signal(|| 0u64);

    rsx! {
        div { class: "chat-layout",
            ConversationSidebar { selected, conversations_changed, pods_changed, turns_changed, questions_changed }
            ChatPanel { selected, conversations_changed, pods_changed, turns_changed, questions_changed }
        }
    }
}

#[component]
fn ChatPanel(
    selected: Memo<Option<i64>>,
    conversations_changed: Signal<u64>,
    pods_changed: Signal<u64>,
    turns_changed: Signal<u64>,
    questions_changed: Signal<u64>,
) -> Element {
    let initial_messages = use_resource(move || {
        let id = selected();
        async move {
            match id {
                Some(id) => Some(get_messages(id).await),
                None => None,
            }
        }
    });

    // The open conversation's state, reset as a whole on a switch.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut state = use_store(ConversationState::default);
    // Its fields, under the names the code below uses.
    let mut messages = state.messages();
    let mut load_error = state.load_error();
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut streaming_reply = state.streaming_reply();
    #[cfg(feature = "web")]
    let mut turn_running = state.turn_running();
    #[cfg(feature = "web")]
    let mut turn_elapsed = state.turn_elapsed();
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut todos = state.todos();
    #[cfg(feature = "web")]
    let mut pending_question = state.pending_question();
    let mut repo_url = state.repo_url();
    let mut repo_branch = state.repo_branch();
    let mut repo_dir = state.repo_dir();
    let mut repo_attaching = state.repo_attaching();
    let mut repo_attach_error = state.repo_attach_error();
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repos = state.repos();
    let sandbox_pods = state.sandbox_pods();
    let sandbox_terminals = state.sandbox_terminals();
    let mut pending_pod_stop = state.pending_pod_stop();
    let mut pod_stop_error = state.pod_stop_error();
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut browsing_session_open = state.browsing_session_open();
    #[cfg(feature = "web")]
    let mut browsing_url = state.browsing_url();
    #[cfg(feature = "web")]
    let mut browsing_frame = state.browsing_frame();
    let address_draft = state.address_draft();
    let mut address_editing = state.address_editing();
    let mut address_pending = state.address_pending();
    let mut address_error = state.address_error();
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut context_usage = state.context_usage();
    let mut layout_snap_pending = state.layout_snap_pending();
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut live = state.live();
    // The server's own "not found", distinct from a failed load: the page
    // says so and offers no message box, instead of a send that can only
    // fail.
    let conversation_missing = move || load_error().as_deref() == Some("conversation not found");
    // The last turn error, by conversation (`TurnError`, or a send the
    // server refused).
    let mut stream_errors: Signal<HashMap<i64, String>> = use_signal(HashMap::new);
    // Bumped when the conversation's model or the providers change, so the
    // model picker refetches (SME-72).
    #[allow(unused_mut)]
    let mut model_changed = use_signal(|| 0u64);
    // Whether the conversation has a model to send to, from the picker:
    // Send waits for one (SME-72). True until the picker knows otherwise.
    let model_ready = use_signal(|| true);
    #[cfg(feature = "web")]
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
    let stop = move |_| {
        let Some(id) = selected() else { return };
        // "Stopped." comes from the notice the stop saves, in every tab and
        // after a reload (SME-51 B10); a stop that fails says so (B11).
        spawn(async move {
            if let Err(e) = crate::api::chat::stop_turn(id).await
                && selected() == Some(id)
            {
                stream_errors
                    .write()
                    .insert(id, format!("Couldn't stop the turn: {}", server_error_message(&e)));
            }
        });
    };
    let mut input = use_signal(String::new);
    let mut next_temp_id = use_signal(|| -1i64);
    // Clones the repo typed into a new conversation's "Work on a repo".
    // Here, not in `RepoAttach`: the form goes as soon as the first message
    // shows, which would drop a clone request still in flight with its
    // reset and its error (SME-57 code review).
    let mut attach_repo_now = move || {
        let Some(id) = selected() else { return };
        if repo_attaching() {
            return;
        }
        let url = repo_url();
        let branch = repo_branch();
        let dir = repo_dir();
        repo_attaching.set(true);
        repo_attach_error.set(None);
        spawn(async move {
            let result = attach_repo(id, url, branch, dir).await;
            // The user may have moved on to another conversation.
            if selected() != Some(id) {
                return;
            }
            match result {
                Ok(_) => {
                    repo_url.set(String::new());
                    repo_branch.set(String::new());
                    repo_dir.set(String::new());
                }
                Err(e) => repo_attach_error.set(Some(server_error_message(&e))),
            }
            repo_attaching.set(false);
        });
    };
    // The sandbox panel's Stop button: click once to arm, again to stop.
    // Here, not in `SandboxPanel`: the panel unmounts once the pod is gone
    // and nothing else shows, which would drop the stop's task before it
    // clears or sets the stop error (SME-57 code review).
    let mut request_pod_stop = move |pod_id: i64| {
        if pending_pod_stop() == Some(pod_id) {
            pending_pod_stop.set(None);
            let conversation = selected();
            spawn(async move {
                let result = crate::api::pods::stop_pod(pod_id).await;
                // Not onto another conversation's panel (SME-51 B11).
                if selected() != conversation {
                    return;
                }
                match result {
                    Ok(()) => pod_stop_error.set(None),
                    Err(e) => pod_stop_error.set(Some(server_error_message(&e))),
                }
            });
        } else {
            pending_pod_stop.set(Some(pod_id));
        }
    };
    // The live frame's width as shown; it scales to fit the panel, and
    // clicks on it are scaled back up to the page's own pixels.
    let frame_shown_width = use_signal(|| FRAME_WIDTH);
    // Goes to the address typed in the browsing panel's address bar.
    // Spawned here, not in `BrowsingPanel`: the panel unmounts when the
    // session closes, which would drop a navigation still loading and
    // leave the address bar disabled (SME-57 code review).
    let mut navigate_address = move || {
        let Some(id) = selected() else { return };
        if address_pending() {
            return;
        }
        let address = address_draft();
        address_pending.set(true);
        address_error.set(None);
        spawn(async move {
            let result = navigate_browser(id, address).await;
            // Not onto another conversation's bar (SME-51 B11).
            if selected() != Some(id) {
                return;
            }
            match result {
                Ok(()) => address_editing.set(false),
                Err(e) => address_error.set(Some(server_error_message(&e))),
            }
            address_pending.set(false);
        });
    };
    // Forwards the live panel's input one request at a time, in the order
    // it happened — separately spawned requests can overtake each other (a
    // mouse-up reaching the page before its mouse-down). Moves that queued
    // up while a request was in flight collapse to the latest one.
    let browser_input = use_coroutine(
        move |mut queue: UnboundedReceiver<(i64, BrowserInputEvent)>| async move {
            while let Ok(first) = queue.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = queue.try_recv() {
                    batch.push(next);
                }
                for (id, event) in coalesce_mouse_moves(batch) {
                    let _ = send_browser_input(id, event).await;
                }
            }
        },
    );
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut tz_offset_minutes: Signal<i32> = use_signal(|| 0);
    // The context bar, so closing its detail view can give it focus back.
    let context_bar_el: Signal<Option<MountedEvent>> = use_signal(|| None);

    // The transcript follows its bottom while the user is at it (see
    // `StickyBottom`); its `onscroll` keeps that current.
    let transcript_scroll = use_sticky_bottom();
    // Whether the pointer is over the transcript, and whether a snap to
    // the bottom that a layout change asked for is waiting for it to
    // leave (SME-75): a side panel appearing mustn't move what the user is
    // about to click. While it waits, the transcript is scrolled to keep
    // the element under the pointer where it was.
    let pointer_over_transcript = use_signal(|| false);
    // Bumped when an image in a reply finishes loading and grows it, a
    // layout change like a panel appearing (SME-30).
    let media_loaded: Signal<u64> = use_signal(|| 0);
    // What the transcript held when the effect below last ran: the message
    // count, the last message's id and the streaming reply's length. Only
    // a change in it is new content; a write that changes nothing (the
    // connect-time pull sets the messages again) is not.
    let mut last_content: Signal<Option<(usize, Option<i64>, Option<usize>)>> = use_signal(|| None);


    // Read once per page load, not per message: timestamps are stored as
    // effectively-UTC `NaiveDateTime`s with no timezone of their own, and
    // the browser's offset is the only place that information can come
    // from. A hook, not an effect: it reads nothing reactive, so an effect
    // never reran anyway (SME-58). Web-only: there's no browser `Date`
    // during SSR, and 0 (UTC) is a fine fallback for the pre-hydration
    // render either way.
    #[cfg(feature = "web")]
    use_hook(move || {
        spawn(async move {
            if let Ok(value) = document::eval("return -new Date().getTimezoneOffset();").await
                && let Some(offset) = value.as_i64()
            {
                tz_offset_minutes.set(offset as i32);
            }
        });
    });

    use_effect(move || match initial_messages() {
        Some(Some(Ok(list))) => {
            if let Some(id) = *selected.peek() {
                let merged = apply_loaded_messages(&messages.peek(), list, id);
                messages.set(merged);
            }
            load_error.set(None);
            // A freshly loaded conversation should open scrolled to its
            // latest message, regardless of where a previous conversation
            // was left scrolled.
            transcript_scroll.stick();
        }
        Some(Some(Err(e))) => load_error.set(Some(server_error_message(&e))),
        Some(None) => {
            messages.set(Vec::new());
            transcript_scroll.stick();
        }
        None => {}
    });

    // Live event subscription: opens once per selected conversation and
    // keeps itself open for as long as that conversation stays selected —
    // independent of, and in addition to, whatever `send_message` calls are
    // in flight. Web-only: SSR has no live browser tab to keep a stream
    // open for, and the server-side executor has no reason to run a loop
    // that never terminates on its own. `event_task` holds the previous
    // subscription's handle so switching conversations cancels it outright
    // (`Task::cancel`) rather than relying on the loop to notice on its own
    // — it might be parked in `events.recv().await` with nothing arriving
    // to wake it back up to check.
    #[cfg(feature = "web")]
    {
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
                        load_error.set(Some(server_error_message(&e)));
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
                        if let Ok(running) = get_turn_state(id).await {
                            turn_running.set(running);
                        }
                        if let Ok(reply) = get_reply_in_progress(id).await {
                            streaming_reply.set(reply);
                        }
                        // A turn that failed before this tab connected
                        // (SME-51 B11).
                        if let Ok(Some(error)) = get_turn_error(id).await {
                            stream_errors.write().insert(id, error);
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
                                    stream_errors.write().insert(id, message);
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

    // A second, independent live subscription — the frame stream isn't
    // part of `ConversationEvent` (see `events::ConversationEvent`'s own
    // doc comment on why frames get a separate channel), and only needs
    // to run while a session is actually open, not for the conversation's
    // whole lifetime the way the main event subscription does. Reacts to
    // both `selected()` (conversation switch) and `browsing_session_open()`
    // (the model opening/closing a session) — either changing tears down
    // any existing subscription and, if a session is now open, starts a
    // fresh one.
    #[cfg(feature = "web")]
    {
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

    let mut send = move || {
        let Some(id) = selected() else { return };
        let content = input();
        if content.trim().is_empty() || !model_ready() {
            return;
        }
        input.set(String::new());

        let temp_id = next_temp_id();
        next_temp_id.set(temp_id - 1);
        #[expect(clippy::expect_used, reason = "a text ContentBlock always serializes")]
        messages.write().push(Message {
            id: temp_id,
            conversation_id: id,
            role: "user".to_string(),
            content: serde_json::to_string(&[ContentBlock::Text {
                text: content.clone(),
            }])
            .expect("ContentBlock always serializes"),
            created_at: chrono::Utc::now().naive_utc(),
        });

        // The reply, and the turn's other messages, arrive on the
        // conversation's own event stream; this request only starts the
        // turn. Keyed by `id`: the viewer may switch away meanwhile.
        spawn(async move {
            stream_errors.write().remove(&id);
            if let Err(e) = send_message(id, content).await {
                stream_errors.write().insert(id, server_error_message(&e));
            }
            // The first message titles a conversation, and any send moves
            // it up the list — refresh the sidebar even if the viewer has
            // switched away (this tab then isn't listening to `id`'s events).
            *conversations_changed.write() += 1;
        });
    };

    // Auto-scroll the transcript to its new bottom whenever a message is
    // added or streaming text grows — but only if the user was already at
    // the bottom (`transcript_scroll`, kept current by the transcript's own
    // `onscroll` handler). Reads the messages and the streaming reply so it
    // reruns on both a persisted message and an in-flight delta; the stuck
    // flag isn't subscribed to (see `StickyBottom::is_stuck`).
    use_effect(move || {
        // Borrowed, not cloned: this reruns on every streamed delta.
        let content = {
            let list = messages.read();
            let reply_len = streaming_reply.read().as_ref().map(String::len);
            (list.len(), list.last().map(|m| m.id), reply_len)
        };
        let changed = *last_content.peek() != Some(content);
        last_content.set(Some(content));
        if !transcript_scroll.is_stuck() {
            return;
        }
        let Some(el) = transcript_scroll.el() else { return };
        // Rerun with nothing new (an unchanged write): only a layout change
        // can have moved the bottom, so it waits on the pointer like one.
        if !changed && *pointer_over_transcript.peek() {
            layout_snap_pending.set(true);
            spawn(keep_transcript_anchor(el, layout_snap_pending));
            return;
        }
        // This reaches the bottom too, so a snap waiting on the pointer
        // has nothing left to do.
        layout_snap_pending.set(false);
        spawn(snap_transcript_for_content(el));
    });

    // The side panels (the sandbox, repos, todos, the browser)
    // render below the transcript in `.side-panels-row`, and appearing or
    // growing shrinks `.chat-main`, after the snap above already ran —
    // leaving `.messages` scrolled to what used to be its bottom. This
    // re-snaps to the new bottom, except while the pointer is over the
    // transcript: there the shift would move whatever the user is about to
    // click (SME-75), so the transcript is scrolled to keep the element
    // under the pointer still, and the snap waits until the pointer leaves
    // (see `onpointerleave` below). Panels beside the chat (a wide window)
    // narrow it; panels above it (a narrow one) push its top edge down, so
    // neither "stay put" nor "snap" alone would keep that element still.
    // New content still follows the bottom with the pointer over it; a
    // growing reply is expected to move.
    use_effect(move || {
        let _ = sandbox_pods();
        let _ = sandbox_terminals();
        let _ = repos();
        let _ = todos();
        let _ = browsing_session_open();
        // The context bar above the transcript appears with the first usage.
        let _ = context_usage();
        // An image in a reply loaded and grew it (SME-30).
        let _ = media_loaded();
        if !transcript_scroll.is_stuck() {
            return;
        }
        let Some(el) = transcript_scroll.el_untracked() else { return };
        if *pointer_over_transcript.peek() {
            layout_snap_pending.set(true);
            spawn(keep_transcript_anchor(el, layout_snap_pending));
            return;
        }
        spawn(scroll_to_bottom(el));
    });

    rsx! {
        section {
            class: "chat-panel",
            "data-live": live().map(|(id, _)| id.to_string()),
            "data-live-pulls": live().map(|(_, pulls)| pulls.to_string()),
            match selected() {
                None => rsx! {
                    div { class: "empty-state", "Select or start a conversation" }
                },
                Some(_) => {
                    rsx! {
                    if !sandbox_pods().is_empty() || !repos().is_empty() || !todos().is_empty() || browsing_session_open() {
                        div { class: "side-panels-row",
                            if browsing_session_open() {
                                BrowsingPanel {
                                    selected,
                                    state,
                                    frame_shown_width,
                                    browser_input,
                                    on_navigate: move |_| navigate_address(),
                                }
                            }
                            if !todos().is_empty() {
                                TodoPanel { state }
                            }
                            if !sandbox_pods().is_empty() || !repos().is_empty() {
                                SandboxPanel {
                                    selected,
                                    state,
                                    on_stop_pod: move |pod_id| request_pod_stop(pod_id),
                                }
                            }
                        }
                    }
                    div { class: "chat-main",
                        ContextUsage {
                            selected,
                            state,
                            context_bar_el,
                        }
                        Transcript {
                            selected,
                            state,
                            initial_messages,
                            tz_offset_minutes,
                            input,
                            on_attach: move |_| attach_repo_now(),
                            stream_errors,
                            scroll: transcript_scroll,
                            pointer_over_transcript,
                            media_loaded,
                        }
                        if !conversation_missing() {
                        if let Some(id) = selected() {
                            super::ModelPicker { key: "{id}", conversation_id: id, refresh: model_changed, ready: model_ready }
                        }
                        Composer {
                            state,
                            input,
                            model_ready,
                            on_send: move |_| send(),
                            on_stop: stop,
                        }
                        }
                    }
                    }
                },
            }
        }
    }
}
