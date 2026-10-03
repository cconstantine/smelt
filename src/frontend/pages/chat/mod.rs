use std::collections::HashMap;

use dioxus::html::geometry::{PixelsVector2D, WheelDelta};
use dioxus::html::input_data::MouseButton;
#[cfg(feature = "web")]
use dioxus::prelude::dioxus_core::Task;
use dioxus::prelude::*;

use super::server_error_message;
use super::TwoStepLabel;
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
#[cfg(feature = "web")]
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
mod panels;
mod question_card;
mod repo_attach;
mod sidebar;
mod state;
mod sticky;
mod streaming;
mod transcript;

use composer::*;
use context::*;
use panels::*;
use question_card::*;
use repo_attach::*;
use sidebar::*;
use state::*;
use sticky::*;
use streaming::*;
use transcript::*;

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

    let mut messages: Signal<Vec<Message>> = use_signal(Vec::new);
    let mut load_error: Signal<Option<String>> = use_signal(|| None);
    // The server's own "not found", distinct from a failed load: the page
    // says so and offers no message box, instead of a send that can only
    // fail.
    let conversation_missing = move || load_error().as_deref() == Some("conversation not found");
    // The open conversation's reply as it streams, from its event stream:
    // `ReplyReset` starts it, `ReplyDelta` adds to it, and the reply being
    // saved (`MessagesAppended`) or the turn ending clears it. Reset on a
    // switch, and restored on (re)connect from `get_reply_in_progress`.
    // Every tab watching the conversation shows it, whoever started the turn.
    #[allow(unused_mut)]
    let mut streaming_reply: Signal<Option<String>> = use_signal(|| None);
    let streaming_text = move || streaming_reply().filter(|text| !text.is_empty());
    // The last turn error, by conversation (`TurnError`, or a send the
    // server refused).
    let mut stream_errors: Signal<HashMap<i64, String>> = use_signal(HashMap::new);
    let stream_error = move || selected().and_then(|id| stream_errors.read().get(&id).cloned());
    // Whether the server has a turn running (or queued) in the selected
    // conversation, from `ConversationEvent::TurnState`: covers turns this
    // tab didn't start (another tab, a finished command waking the model).
    #[allow(unused_mut)]
    let mut turn_running = use_signal(|| false);
    // Bumped when the conversation's model or the providers change, so the
    // model picker refetches (SME-72).
    #[allow(unused_mut)]
    let mut model_changed = use_signal(|| 0u64);
    // Whether the conversation has a model to send to, from the picker:
    // Send waits for one (SME-72). True until the picker knows otherwise.
    let model_ready = use_signal(|| true);
    // The message box waits while the model works in this conversation,
    // whoever started the turn; Stop is offered instead.
    let is_streaming = move || turn_running();
    let can_stop = move || turn_running();
    // Seconds this tab has seen the current turn running, for the
    // "Working…" line (SME-41 D1). Ticks once a second while a turn runs,
    // and resets when it ends. Web only.
    #[allow(unused_mut)]
    let mut turn_elapsed = use_signal(|| 0u64);
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
    // Set when a background wake-up (a terminal command finishing with no
    // `send_message` call in flight) fails to actually reach the model —
    // see `ConversationEvent::NotificationDeliveryFailed`. Separate from
    // `stream_errors` since that one's reset at the start of every `send()`
    // call; this can arrive at any time, not tied to a live send.
    //
    // `mut` is only exercised by the `web`-only live-subscription loop and
    // timezone effect below (`.set()`/`.write()`); a `server`-only build
    // never mutates these, so `allow(unused_mut)` there.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut notification_delivery_error: Signal<Option<String>> = use_signal(|| None);
    let mut input = use_signal(String::new);
    let mut next_temp_id = use_signal(|| -1i64);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut todos: Signal<Vec<TodoItem>> = use_signal(Vec::new);
    // The question this conversation waits on, for its card (SME-34).
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut pending_question: Signal<Option<PendingQuestion>> = use_signal(|| None);
    // "Work on a repo" in a new conversation.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repo_url = use_signal(String::new);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repo_branch = use_signal(String::new);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repo_dir = use_signal(String::new);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repo_attaching = use_signal(|| false);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repo_attach_error: Signal<Option<String>> = use_signal(|| None);
    // The last Trust / Don't trust / Reload failure.
    let mut repo_action_error: Signal<Option<String>> = use_signal(|| None);
    // The conversation's git repos (SME-32), from `ReposUpdate`.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut repos: Signal<Vec<RepoSummary>> = use_signal(Vec::new);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut sandbox_pods: Signal<Vec<SandboxPodPanelEntry>> = use_signal(Vec::new);
    // The panel's Stop button: the pod armed for stopping (click once to
    // arm, again to confirm), and the last stop's error. The pod itself
    // disappears through the usual `SandboxPodUpdate` event.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut pending_pod_stop: Signal<Option<i64>> = use_signal(|| None);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut pod_stop_error: Signal<Option<String>> = use_signal(|| None);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut sandbox_terminals: Signal<Vec<SandboxTerminalPanelEntry>> = use_signal(Vec::new);
    // Whether the model currently has a browsing session open — drives
    // both the panel's visibility and (via the separate effect below)
    // the live frame subscription. Updated by the initial
    // `get_browsing_state` pull and by `ConversationEvent::BrowsingSessionUpdate`.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut browsing_session_open: Signal<bool> = use_signal(|| false);
    // The session page's current URL, from `get_browsing_state` and
    // `ConversationEvent::BrowsingUrlUpdate`.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut browsing_url: Signal<Option<String>> = use_signal(|| None);
    // Set while this tab is live on the conversation: subscribed to its
    // events and done with the snapshot pull that follows. The id, and how
    // many times this page has connected and pulled (more than one means a
    // reconnect). Shown as `data-live`/`data-live-pulls` on the panel, which
    // the browser tests wait for (SME-59).
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut live: Signal<Option<(i64, u32)>> = use_signal(|| None);
    // The address bar's own state: what's typed, whether the viewer is
    // typing (so incoming URL changes don't clobber it), an in-flight
    // navigation, and the last navigation error.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut address_draft = use_signal(String::new);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut address_editing = use_signal(|| false);
    // The live frame's width as shown; it scales to fit the panel, and
    // clicks on it are scaled back up to the page's own pixels.
    let frame_shown_width = use_signal(|| FRAME_WIDTH);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut address_pending = use_signal(|| false);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut address_error: Signal<Option<String>> = use_signal(|| None);
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
    // The latest live-panel frame (base64 JPEG), `None` until the first
    // one arrives after subscribing.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut browsing_frame: Signal<Option<String>> = use_signal(|| None);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut tz_offset_minutes: Signal<i32> = use_signal(|| 0);
    // `None` until the first `ContextUsageUpdate`/`get_context_usage` pull
    // — a brand-new conversation has no turn yet to report usage for. See
    // SME-18.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut context_usage: Signal<Option<ContextUsageSnapshot>> = use_signal(|| None);
    // The click-through detail view: closed by default, fetched on demand
    // (not kept live) the moment it's opened — see `open_context_detail`.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut context_detail: Signal<Option<ContextDetailSnapshot>> = use_signal(|| None);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut context_detail_open = use_signal(|| false);
    // The context bar, so closing its detail view can give it focus back.
    let context_bar_el: Signal<Option<MountedEvent>> = use_signal(|| None);

    // Sticky-bottom auto-scroll state for the message transcript: the
    // mounted `.messages` element (so an effect can query/set its scroll
    // position) and whether it was at the bottom the last time the user
    // scrolled it — read, not written, by the auto-scroll effect below;
    // written only by the `onscroll` handler on the element itself, so it
    // always reflects a real user (or auto-scroll-induced) scroll position
    // rather than the reactive-render cycle.
    let mut messages_el: Signal<Option<MountedEvent>> = use_signal(|| None);
    let mut messages_stuck_to_bottom = use_signal(|| true);
    // Whether the pointer is over the transcript, and whether a snap to
    // the bottom that a layout change asked for is waiting for it to
    // leave (SME-75): a side panel appearing mustn't move what the user is
    // about to click. While it waits, the transcript is scrolled to keep
    // the element under the pointer where it was.
    let mut pointer_over_transcript = use_signal(|| false);
    let mut layout_snap_pending = use_signal(|| false);
    // What the transcript held when the effect below last ran: the message
    // count, the last message's id and the streaming reply's length. Only
    // a change in it is new content; a write that changes nothing (the
    // connect-time pull sets the messages again) is not.
    let mut last_content: Signal<Option<(usize, Option<i64>, Option<usize>)>> = use_signal(|| None);

    // Same idea, per sandbox terminal — each terminal's own
    // `.task-terminal-body` scrolls independently, like `tail -f` on its own
    // log, so each needs its own mounted handle and stuck flag.
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut terminal_body_els: Signal<HashMap<i64, MountedEvent>> = use_signal(HashMap::new);
    #[cfg_attr(not(feature = "web"), allow(unused_mut))]
    let mut terminal_body_stuck: Signal<HashMap<i64, bool>> = use_signal(HashMap::new);

    // Fetched once per page load (this effect reads no reactive signal, so
    // it never re-runs), not per message — timestamps are stored as
    // effectively-UTC `NaiveDateTime`s with no timezone of their own, and
    // the browser's offset is the only place that information can come
    // from. Web-only: there's no browser `Date` during SSR, and 0 (UTC) is
    // a fine fallback for the pre-hydration render either way.
    #[cfg(feature = "web")]
    use_effect(move || {
        spawn(async move {
            if let Ok(value) = document::eval("return -new Date().getTimezoneOffset();").await {
                if let Some(offset) = value.as_i64() {
                    tz_offset_minutes.set(offset as i32);
                }
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
            messages_stuck_to_bottom.set(true);
        }
        Some(Some(Err(e))) => load_error.set(Some(server_error_message(&e))),
        Some(None) => {
            messages.set(Vec::new());
            messages_stuck_to_bottom.set(true);
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
            let Some(id) = selected() else { return };
            todos.set(Vec::new());
            repos.set(Vec::new());
            // "Work on a repo" and the trust cards belong to the conversation
            // they were used in (SME-32 code review 8).
            repo_url.set(String::new());
            repo_branch.set(String::new());
            repo_dir.set(String::new());
            repo_attaching.set(false);
            repo_attach_error.set(None);
            repo_action_error.set(None);
            layout_snap_pending.set(false);
            sandbox_pods.set(Vec::new());
            sandbox_terminals.set(Vec::new());
            terminal_body_els.write().clear();
            terminal_body_stuck.write().clear();
            browsing_session_open.set(false);
            browsing_frame.set(None);
            browsing_url.set(None);
            address_editing.set(false);
            address_error.set(None);
            notification_delivery_error.set(None);
            turn_running.set(false);
            streaming_reply.set(None);
            context_usage.set(None);
            // The rest of what belonged to the conversation left (SME-51 B11).
            messages.set(Vec::new());
            load_error.set(None);
            turn_elapsed.set(0);
            pending_pod_stop.set(None);
            pod_stop_error.set(None);
            context_detail.set(None);
            context_detail_open.set(false);
            pending_question.set(None);
            address_pending.set(false);
            address_draft.set(String::new());

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
                            merge_sandbox_snapshot(
                                &mut sandbox_pods.write(),
                                &mut sandbox_terminals.write(),
                                snapshot,
                            );
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
                            match events.recv().await {
                                Some(Ok(ConversationEvent::MessagesAppended { messages: rows })) => {
                                    *conversations_changed.write() += 1;
                                    // A saved reply replaces its streaming copy.
                                    if rows.iter().any(|m| m.role == "assistant") {
                                        streaming_reply.set(None);
                                    }
                                    accept_saved_messages(&mut messages.write(), rows);
                                }
                                Some(Ok(ConversationEvent::SandboxPodUpdate {
                                    pod_id,
                                    status,
                                    terminated,
                                })) => {
                                    apply_sandbox_pod_update(
                                        &mut sandbox_pods.write(),
                                        &mut sandbox_terminals.write(),
                                        pod_id,
                                        status,
                                        terminated,
                                    );
                                }
                                Some(Ok(ConversationEvent::SandboxPreviewUpdate { pod_id, previews })) => {
                                    apply_sandbox_preview_update(&mut sandbox_pods.write(), pod_id, previews);
                                }
                                Some(Ok(ConversationEvent::SandboxTerminalUpdate {
                                    pod_id,
                                    terminal_id,
                                    status,
                                    terminated,
                                })) => {
                                    apply_sandbox_terminal_update(
                                        &mut sandbox_terminals.write(),
                                        pod_id,
                                        terminal_id,
                                        status,
                                        terminated,
                                    );
                                }
                                Some(Ok(ConversationEvent::SandboxCommandUpdate {
                                    terminal_id,
                                    command_id,
                                    command,
                                    status,
                                    exit_code,
                                    stream,
                                    latest_output,
                                    position,
                                })) => {
                                    apply_sandbox_command_update(
                                        &mut sandbox_terminals.write(),
                                        terminal_id,
                                        command_id,
                                        command,
                                        status,
                                        exit_code,
                                        stream,
                                        latest_output,
                                        position,
                                    );
                                }
                                Some(Ok(ConversationEvent::NotificationDeliveryFailed {
                                    detail,
                                })) => {
                                    notification_delivery_error.set(Some(detail));
                                }
                                Some(Ok(ConversationEvent::ContextUsageUpdate {
                                    usage,
                                    context_window,
                                })) => {
                                    context_usage.set(Some(ContextUsageSnapshot {
                                        usage: Some(usage),
                                        context_window,
                                    }));
                                }
                                Some(Ok(ConversationEvent::TodoListUpdate { items })) => {
                                    todos.set(items);
                                }
                                Some(Ok(ConversationEvent::BrowsingSessionUpdate { open })) => {
                                    browsing_session_open.set(open);
                                    if !open {
                                        browsing_frame.set(None);
                                        browsing_url.set(None);
                                    }
                                }
                                Some(Ok(ConversationEvent::BrowsingUrlUpdate { url })) => {
                                    browsing_url.set(Some(url));
                                }
                                Some(Ok(ConversationEvent::PodsChanged {})) => {
                                    *pods_changed.write() += 1;
                                }
                                Some(Ok(ConversationEvent::TurnsChanged {})) => {
                                    *turns_changed.write() += 1;
                                }
                                Some(Ok(ConversationEvent::QuestionUpdate { question })) => {
                                    pending_question.set(question);
                                }
                                Some(Ok(ConversationEvent::QuestionsChanged {})) => {
                                    *questions_changed.write() += 1;
                                }
                                Some(Ok(ConversationEvent::ModelChanged {} | ConversationEvent::ProvidersChanged {})) => {
                                    *model_changed.write() += 1;
                                    // Another model can mean another context
                                    // window: the meter measures against it.
                                    spawn(async move {
                                        if let Ok(snapshot) = get_context_usage(id).await
                                            && selected() == Some(id)
                                        {
                                            context_usage.set(Some(snapshot));
                                        }
                                    });
                                }
                                Some(Ok(ConversationEvent::TurnState { running })) => {
                                    turn_running.set(running);
                                    if !running {
                                        streaming_reply.set(None);
                                    }
                                }
                                Some(Ok(ConversationEvent::ReplyReset {})) => {
                                    streaming_reply.set(Some(String::new()));
                                }
                                Some(Ok(ConversationEvent::ReplyDelta { text, offset })) => {
                                    apply_reply_delta(&mut streaming_reply.write(), offset, &text);
                                }
                                // A stop shows as its saved notice instead.
                                Some(Ok(ConversationEvent::TurnError { message }))
                                    if message != crate::api::chat::TURN_STOPPED =>
                                {
                                    stream_errors.write().insert(id, message);
                                }
                                Some(Ok(ConversationEvent::TurnError { .. })) => {}
                                Some(Ok(ConversationEvent::ReposUpdate { repos: list })) => {
                                    repos.set(list);
                                }
                                // A type added since this page loaded: the
                                // server is newer than this bundle.
                                Some(Ok(ConversationEvent::Unknown)) => {
                                    *crate::frontend::STALE_BUNDLE.write() = true;
                                }
                                Some(Err(_)) | None => break,
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
            if !browsing_session_open() {
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
    // the bottom (`messages_stuck_to_bottom`, kept current by the
    // `.messages` div's own `onscroll` handler below). Reads `messages()`
    // and `streaming_reply()` so it reruns on both a persisted message and
    // an in-flight delta. The stuck flag itself is read with `peek()`:
    // `onscroll` sets it on every scroll event (a `set` notifies even when
    // the value is unchanged), so reading it reactively reran this on each
    // one and pulled a scroll that stayed within the slack back to the
    // bottom (SME-83).
    use_effect(move || {
        let content = {
            let list = messages();
            let reply = streaming_reply();
            (list.len(), list.last().map(|m| m.id), reply.as_ref().map(String::len))
        };
        let changed = *last_content.peek() != Some(content);
        last_content.set(Some(content));
        if !*messages_stuck_to_bottom.peek() {
            return;
        }
        let Some(el) = messages_el() else { return };
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
        if !*messages_stuck_to_bottom.peek() {
            return;
        }
        let Some(el) = messages_el.peek().clone() else { return };
        if *pointer_over_transcript.peek() {
            layout_snap_pending.set(true);
            spawn(keep_transcript_anchor(el, layout_snap_pending));
            return;
        }
        spawn(scroll_to_bottom(el));
    });

    // Same sticky-bottom behavior, per sandbox terminal — each terminal's
    // body scrolls independently as its own output grows. A terminal with
    // no recorded stuck state yet (just appeared) defaults to stuck, same as
    // the transcript on first load. The stuck map is peeked, as above, so a
    // scroll doesn't rerun this (SME-83).
    use_effect(move || {
        let current_terminals = sandbox_terminals();
        let els = terminal_body_els();
        let stuck = terminal_body_stuck.peek().clone();
        for terminal in current_terminals {
            if !stuck.get(&terminal.terminal_id).copied().unwrap_or(true) {
                continue;
            }
            let Some(el) = els.get(&terminal.terminal_id).cloned() else {
                continue;
            };
            spawn(async move {
                if let Ok(size) = el.get_scroll_size().await {
                    let _ = el
                        .scroll(
                            PixelsVector2D::new(0.0, size.height),
                            ScrollBehavior::Instant,
                        )
                        .await;
                }
            });
        }
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
                    let tool_names = tool_use_names_by_id(&messages());
                    let tool_results = tool_results_by_id(&messages());
                    let commands = terminal_commands_by_id(&messages());
                    rsx! {
                    if !sandbox_pods().is_empty() || !repos().is_empty() || !todos().is_empty() || browsing_session_open() {
                        div { class: "side-panels-row",
                            if browsing_session_open() {
                                BrowsingPanel {
                                    selected,
                                    browsing_url,
                                    browsing_frame,
                                    address_draft,
                                    address_editing,
                                    address_pending,
                                    address_error,
                                    frame_shown_width,
                                    browser_input,
                                }
                            }
                            if !todos().is_empty() {
                                TodoPanel { todos }
                            }
                            if !sandbox_pods().is_empty() || !repos().is_empty() {
                                SandboxPanel {
                                    selected,
                                    repos,
                                    sandbox_pods,
                                    sandbox_terminals,
                                    pending_pod_stop,
                                    pod_stop_error,
                                    terminal_body_els,
                                    terminal_body_stuck,
                                }
                            }
                        }
                    }
                    div { class: "chat-main",
                        ContextUsage {
                            selected,
                            context_usage,
                            context_detail,
                            context_detail_open,
                            context_bar_el,
                        }
                        div {
                            class: "messages",
                            onmounted: move |evt| {
                                messages_el.set(Some(evt));
                                spawn(async move {
                                    let _ = document::eval(TRANSCRIPT_ANCHOR_SETUP).await;
                                });
                            },
                            onpointerenter: move |_| pointer_over_transcript.set(true),
                            // Also on a move, in case the transcript appeared
                            // under a pointer that was already there.
                            onpointermove: move |_| {
                                if !*pointer_over_transcript.peek() {
                                    pointer_over_transcript.set(true);
                                }
                            },
                            onpointerleave: move |_| {
                                pointer_over_transcript.set(false);
                                if *layout_snap_pending.peek() {
                                    layout_snap_pending.set(false);
                                    if *messages_stuck_to_bottom.peek()
                                        && let Some(el) = messages_el.peek().clone()
                                    {
                                        spawn(scroll_to_bottom(el));
                                    }
                                }
                            },
                            // A scroll the user starts while a snap is pending
                            // gives up the snap, so the scroll that follows
                            // decides whether they're still at the bottom.
                            onwheel: move |_| layout_snap_pending.set(false),
                            // A press starts a scrollbar or selection drag,
                            // or focuses text the keys then scroll.
                            onpointerdown: move |_| layout_snap_pending.set(false),
                            ontouchmove: move |_| layout_snap_pending.set(false),
                            onkeydown: move |_| layout_snap_pending.set(false),
                            onscroll: move |evt: Event<ScrollData>| {
                                // While a snap is pending, the scrolls are the
                                // transcript keeping the text under the pointer
                                // still, not the user leaving the bottom: the
                                // snap still owes them the bottom (SME-75 code
                                // review).
                                if *layout_snap_pending.peek() {
                                    return;
                                }
                                let d = evt.data();
                                messages_stuck_to_bottom
                                    .set(
                                        is_scrolled_to_bottom(
                                            d.scroll_top(),
                                            d.scroll_height() as f64,
                                            d.client_height() as f64,
                                        ),
                                    );
                            },
                            if conversation_missing() {
                                p { class: "conversation-missing",
                                    "This conversation doesn't exist. It may have been deleted."
                                }
                            } else if let Some(err) = load_error() {
                                p { class: "error", "Error loading messages: {err}" }
                            }
                            for message in messages() {
                                match message.blocks() {
                                    Ok(blocks) => {
                                        let thinking_open = reply_is_only_thinking(&message.role, &blocks);
                                        rsx! {
                                            for (i , block) in blocks.iter().enumerate() {
                                                {render_block_element(message.id, i, &message.role, message.created_at, tz_offset_minutes(), block, &tool_names, &tool_results, &commands, thinking_open)}
                                            }
                                        }
                                    },
                                    Err(e) => rsx! {
                                        div {
                                            key: "{message.id}",
                                            class: "message message-{message.role} message-error",
                                            "Error rendering message: {e}"
                                        }
                                    },
                                }
                            }
                            // A new conversation says what smelt does and offers a
                            // few asks to start from, instead of a blank screen
                            // (SME-41 D12). Picking one fills the message box.
                            if messages().is_empty() && !turn_running() && !conversation_missing() && matches!(initial_messages(), Some(Some(Ok(_)))) {
                                div { class: "conversation-empty",
                                    h2 { "What should smelt work on?" }
                                    p { "It works in a sandbox of its own: it writes and runs code, uses a terminal, reads the web, and shows you each step." }
                                    RepoAttach {
                                        selected,
                                        repo_url,
                                        repo_branch,
                                        repo_dir,
                                        repo_attaching,
                                        repo_attach_error,
                                    }
                                    div { class: "example-asks",
                                        for example in EXAMPLE_ASKS {
                                            button {
                                                class: "example-ask",
                                                r#type: "button",
                                                onclick: move |_| input.set(example.to_string()),
                                                "{example}"
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(reply) = streaming_text() {
                                div { class: "message message-assistant message-streaming", "{reply}" }
                            }
                            if turn_running() {
                                div { class: "turn-working", role: "status",
                                    span { class: "turn-working-dot" }
                                    span { "Working… {format_elapsed(turn_elapsed())}" }
                                }
                            }
                            if let Some(err) = stream_error() {
                                if err == crate::providers::NO_MODEL_CONFIGURED {
                                    p { class: "model-picker-setup", role: "status",
                                        "{err} "
                                        Link { to: Route::ProvidersRoute {}, "Model providers" }
                                    }
                                } else {
                                    p { class: "error", "{err}" }
                                }
                            }
                            if let Some(err) = notification_delivery_error() {
                                if err == crate::providers::NO_MODEL_CONFIGURED {
                                    p { class: "model-picker-setup", role: "status",
                                        "A finished command or task is waiting for the model, but there's no model to send it to. "
                                        Link { to: Route::ProvidersRoute {}, "Model providers" }
                                    }
                                } else {
                                    p { class: "error", "A background notification failed to reach the model: {err}" }
                                }
                            }
                            // A repo's AGENTS.md waits for the user's trust before it
                            // becomes instructions the model follows (SME-32).
                            for (repo, request) in repos().into_iter().flat_map(|r| r.trust_requests.clone().into_iter().map(move |q| (r.clone(), q))) {
                                div { key: "trust-{request.id}", class: "trust-card", role: "group", aria_label: "Trust {repo.url}?",
                                    p { class: "trust-card-question",
                                        "Trust "
                                        code { "{repo.url}" }
                                        "?"
                                    }
                                    p { class: "muted",
                                        "The model wants to load this AGENTS.md as instructions it follows on every turn. Trust loads exactly the file below, and later ones from this repo load without asking. Only trust repos whose instructions you're happy for the model to follow."
                                    }
                                    details { class: "trust-card-preview", open: true,
                                        summary { "{request.path}" }
                                        pre { "{request.content}" }
                                    }
                                    div { class: "trust-card-buttons",
                                        button {
                                            class: "trust-card-trust",
                                            r#type: "button",
                                            onclick: {
                                                let request_id = request.id;
                                                let shown_hash = request.hash.clone();
                                                move |_| {
                                                    let Some(id) = selected() else { return };
                                                    let shown_hash = shown_hash.clone();
                                                    repo_action_error.set(None);
                                                    spawn(async move {
                                                        if let Err(e) = decide_repo_trust(id, request_id, shown_hash, true).await
                                                            // Not if the user has moved on to another conversation.
                                                            && selected() == Some(id)
                                                        {
                                                            repo_action_error.set(Some(server_error_message(&e)));
                                                        }
                                                    });
                                                }
                                            },
                                            "Trust"
                                        }
                                        button {
                                            class: "trust-card-decline",
                                            r#type: "button",
                                            onclick: {
                                                let request_id = request.id;
                                                let shown_hash = request.hash.clone();
                                                move |_| {
                                                    let Some(id) = selected() else { return };
                                                    let shown_hash = shown_hash.clone();
                                                    repo_action_error.set(None);
                                                    spawn(async move {
                                                        if let Err(e) = decide_repo_trust(id, request_id, shown_hash, false).await
                                                            // Not if the user has moved on to another conversation.
                                                            && selected() == Some(id)
                                                        {
                                                            repo_action_error.set(Some(server_error_message(&e)));
                                                        }
                                                    });
                                                }
                                            },
                                            "Don't trust"
                                        }
                                    }
                                }
                            }
                            if let Some(err) = repo_action_error() {
                                p { class: "error", "{err}" }
                            }
                            // The model's question, waiting on the user (SME-34).
                            if let (Some(id), Some(question)) = (selected(), pending_question()) {
                                QuestionCard { key: "{question.tool_use_id}", conversation_id: id, question }
                            }
                        }
                        if !conversation_missing() {
                        if let Some(id) = selected() {
                            super::ModelPicker { key: "{id}", conversation_id: id, refresh: model_changed, ready: model_ready }
                        }
                        form {
                            class: "composer",
                            onsubmit: move |event| {
                                event.prevent_default();
                                send();
                            },
                            input {
                                r#type: "text",
                                value: "{input}",
                                disabled: is_streaming(),
                                placeholder: if pending_question().is_some() { "Answer the question above, or write a reply instead" } else { "Type a message..." },
                                oninput: move |e| input.set(e.value()),
                            }
                            button {
                                r#type: "submit",
                                disabled: is_streaming() || !model_ready(),
                                title: if model_ready() { "" } else { "Choose a model first" },
                                "Send"
                            }
                            if can_stop() {
                                button {
                                    r#type: "button",
                                    class: "stop-turn",
                                    title: "Stop the model's current turn. Its sandbox, terminals and running commands keep going.",
                                    onclick: stop,
                                    "Stop"
                                }
                            }
                        }
                        }
                    }
                    }
                },
            }
        }
    }
}
