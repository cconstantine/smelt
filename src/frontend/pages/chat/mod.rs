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

mod actions;
mod composer;
mod context;
mod follow;
mod model_setup;
mod panels;
mod live;
mod question_card;
mod repo_attach;
mod shown_text;
mod sidebar;
mod state;
mod sticky;
mod streaming;
mod transcript;
mod trust_card;

use actions::*;
use composer::*;
use context::*;
use follow::*;
use model_setup::*;
use panels::*;
use live::*;
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
    // Tagged with its conversation (SME-185 B38): a restarted resource
    // keeps its last value until the new load lands, so on a switch it
    // still holds the conversation left. The id tells the two apart.
    let initial_messages = use_resource(move || {
        let id = selected();
        async move {
            match id {
                Some(id) => Some((id, get_messages(id).await)),
                None => None,
            }
        }
    });

    // The open conversation's state, reset as a whole on a switch.
    let state = use_store(ConversationState::default);
    // The fields the markup below reads.
    let load_error = state.load_error();
    let todos = state.todos();
    let repos = state.repos();
    let sandbox_pods = state.sandbox_pods();
    let browsing_session_open = state.browsing_session_open();
    let live = state.live();
    // The server's own "not found", distinct from a failed load: the page
    // says so and offers no message box, instead of a send that can only
    // fail.
    let conversation_missing = move || load_error().as_deref() == Some("conversation not found");
    // The error line, by conversation: a turn's (`TurnError`) or one only
    // this tab saw (a send or Stop the server refused); see `ChatErrors`.
    let stream_errors: Signal<ChatErrors> = use_signal(ChatErrors::default);
    // "Work on a repo"'s clone in flight and its error, by conversation
    // (SME-263); see `RepoAttaches`.
    let attaches: Signal<RepoAttaches> = use_signal(RepoAttaches::default);
    // Bumped when the conversation's model or the providers change, so the
    // model picker refetches (SME-72).
    let model_changed = use_signal(|| 0u64);
    // Whether the conversation has a model to send to, from the picker:
    // Send waits for one (SME-72). True until the picker knows otherwise.
    let model_ready = use_signal(|| true);
    #[cfg(feature = "web")]
    use_turn_timer(state);
    let stop = stop_action(selected, stream_errors);
    let input = use_signal(String::new);
    let next_temp_id = use_signal(|| -1i64);
    let mut attach_repo_now = attach_action(selected, state, attaches);
    let mut request_pod_stop = pod_stop_action(selected, state);
    // The live frame's width as shown; it scales to fit the panel, and
    // clicks on it are scaled back up to the page's own pixels.
    let frame_shown_width = use_signal(|| FRAME_WIDTH);
    let mut navigate_address = navigate_action(selected, state);
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
    let tz_offset_minutes: Signal<i32> = use_signal(|| 0);
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

    #[cfg(feature = "web")]
    use_timezone_offset(tz_offset_minutes);
    use_initial_messages(initial_messages, selected, state, transcript_scroll);
    #[cfg(feature = "web")]
    use_conversation_events(
        selected,
        state,
        stream_errors,
        conversations_changed,
        pods_changed,
        turns_changed,
        questions_changed,
        model_changed,
    );
    #[cfg(feature = "web")]
    use_browser_frames(selected, state);
    let mut send = send_action(selected, state, input, next_temp_id, model_ready, stream_errors, conversations_changed, attaches);
    use_transcript_follow(state, transcript_scroll, pointer_over_transcript, media_loaded);

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
                            attaches,
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
