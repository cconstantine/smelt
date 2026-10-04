//! The conversation list.

use super::*;

/// How long ago a conversation was last active, as the sidebar shows it:
/// one unit, `now`, `5m`, `3h`, `2d`, `3w` (SME-41 D8).
pub(super) fn short_age(seconds: i64) -> String {
    match seconds {
        s if s < 60 => "now".to_string(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s => format!("{}w", s / (7 * 86_400)),
    }
}

#[component]
pub(super) fn ConversationSidebar(
    selected: Memo<Option<i64>>,
    conversations_changed: Signal<u64>,
    pods_changed: Signal<u64>,
    turns_changed: Signal<u64>,
    questions_changed: Signal<u64>,
) -> Element {
    let navigator = use_navigator();
    // Which conversations have a live pod, for the dot next to their title.
    let live_pods = use_resource(move || {
        let _ = pods_changed();
        crate::api::pods::get_live_pod_conversations()
    });
    let has_live_pod = move |id: i64| {
        matches!(&*live_pods.read(), Some(Ok(ids)) if ids.contains(&id))
    };
    // Which conversations have a turn running, marked as working
    // (SME-41 D9).
    let busy = use_resource(move || {
        let _ = turns_changed();
        crate::api::chat::get_busy_conversations()
    });
    let is_busy = move |id: i64| matches!(&*busy.read(), Some(Ok(ids)) if ids.contains(&id));
    // Which conversations wait on the user's answer to the model's question
    // (SME-34).
    let waiting = use_resource(move || {
        let _ = questions_changed();
        get_waiting_conversations()
    });
    let is_waiting = move |id: i64| matches!(&*waiting.read(), Some(Ok(ids)) if ids.contains(&id));
    let initial_conversations = use_resource(move || {
        let _ = conversations_changed();
        get_conversations()
    });
    let mut conversations: Signal<Vec<Conversation>> = use_signal(Vec::new);
    let mut loaded = use_signal(|| false);
    // Why the list couldn't load, and why the last New conversation or
    // Delete failed. Each is cleared by its own next success: the list
    // reloads on every new message, which says nothing about a failed
    // Delete (SME-81).
    let mut list_error: Signal<Option<String>> = use_signal(|| None);
    let mut action_error: Signal<Option<String>> = use_signal(|| None);
    let mut pending_delete: Signal<Option<i64>> = use_signal(|| None);
    // Whether the conversation list is open on a phone, where it folds
    // behind a button (SME-41 D4). Ignored at wider widths.
    let mut open_on_phone = use_signal(|| false);
    // The browser's clock, refreshed every minute, for each row's age
    // (SME-41 D8). None until the first reading, when ages aren't shown.
    #[allow(unused_mut)]
    let mut now_utc: Signal<Option<chrono::NaiveDateTime>> = use_signal(|| None);
    #[cfg(feature = "web")]
    use_hook(move || {
        spawn(async move {
            loop {
                if let Ok(value) = document::eval("return Date.now();").await
                    && let Some(ms) = value.as_f64()
                {
                    now_utc.set(chrono::DateTime::from_timestamp_millis(ms as i64).map(|t| t.naive_utc()));
                }
                gloo_timers::future::TimeoutFuture::new(60_000).await;
            }
        });
    });

    use_effect(move || {
        if let Some(result) = initial_conversations() {
            match result {
                Ok(list) => {
                    conversations.set(list);
                    list_error.set(None);
                }
                Err(e) => list_error.set(Some(server_error_message(&e))),
            }
            loaded.set(true);
        }
    });

    let new_conversation = move |_: Event<MouseData>| {
        spawn(async move {
            match create_conversation().await {
                Ok(conversation) => {
                    action_error.set(None);
                    let id = conversation.id;
                    conversations.write().insert(0, conversation);
                    navigator.push(Route::ConversationRoute { id });
                }
                Err(e) => action_error.set(Some(server_error_message(&e))),
            }
        });
    };

    // First click on a row's delete button arms it; a second click on the
    // same (still-armed) row confirms. Only one row is ever armed at a
    // time, so arming a different row implicitly cancels the last one.
    let mut request_delete = move |id: i64| {
        if pending_delete() == Some(id) {
            pending_delete.set(None);
            spawn(async move {
                match delete_conversation(id).await {
                    Ok(()) => {
                        action_error.set(None);
                        conversations.write().retain(|c| c.id != id);
                        if selected() == Some(id) {
                            navigator.push(Route::Home {});
                        }
                    }
                    Err(e) => action_error.set(Some(server_error_message(&e))),
                }
            });
        } else {
            pending_delete.set(Some(id));
        }
    };

    rsx! {
        aside { class: if open_on_phone() { "sidebar sidebar-open" } else { "sidebar" },
            // Phone only (see chat.css): the list folds behind this, so the
            // conversation, not the list, fills the screen (SME-41 D4).
            button {
                class: "sidebar-toggle",
                r#type: "button",
                aria_expanded: "{open_on_phone()}",
                onclick: move |_| open_on_phone.toggle(),
                if open_on_phone() { "Close" } else { "Conversations" }
            }
            div { class: "sidebar-body",
            button { class: "new-conversation", onclick: move |e| { open_on_phone.set(false); new_conversation(e) }, "New conversation" }
            Link { to: Route::ProvidersRoute {}, class: "sandbox-volumes-link providers-link", "Model providers" }
            Link { to: Route::McpServersRoute {}, class: "mcp-servers-link", "MCP servers" }
            Link { to: Route::PodsRoute {}, class: "pods-link", "Sandboxes" }
            Link { to: Route::SandboxVolumesRoute {}, class: "sandbox-volumes-link", "Sandbox volumes" }
            Link { to: Route::GitRoute {}, class: "sandbox-volumes-link git-link", "Git" }
            Link { to: Route::LanguageServersRoute {}, class: "sandbox-volumes-link language-servers-link", "Language servers" }
            if let Some(err) = list_error() {
                p { class: "error", "{err}" }
            }
            if let Some(err) = action_error() {
                p { class: "error", "{err}" }
            }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else if conversations().is_empty() {
                p { class: "muted", "No conversations yet" }
            } else {
                div { class: "conversation-list",
                    for conversation in conversations() {
                        div {
                            key: "{conversation.id}",
                            "data-conversation-id": "{conversation.id}",
                            class: if selected() == Some(conversation.id) { "conversation-item active" } else { "conversation-item" },
                            onclick: move |_| {
                                pending_delete.set(None);
                                open_on_phone.set(false);
                                navigator.push(Route::ConversationRoute { id: conversation.id });
                            },
                            span { class: "conversation-title", "{conversation.title}" }
                            if is_busy(conversation.id) {
                                span { class: "conversation-busy", title: "Working" }
                            }
                            if is_waiting(conversation.id) {
                                span { class: "conversation-waiting", title: "Waiting for your answer", "?" }
                            }
                            if has_live_pod(conversation.id) {
                                span { class: "live-pod-dot", title: "sandbox pod running" }
                            }
                            if let Some(now) = now_utc() {
                                span { class: "conversation-age", {short_age((now - conversation.updated_at).num_seconds())} }
                            }
                            super::TwoStepButton {
                                armed: pending_delete() == Some(conversation.id),
                                class: "delete-conversation",
                                idle: "Delete",
                                confirm: "Confirm?",
                                on_arm: move |_| request_delete(conversation.id),
                                on_confirm: move |_| request_delete(conversation.id),
                            }
                        }
                    }
                }
            }
            }
        }
    }
}
