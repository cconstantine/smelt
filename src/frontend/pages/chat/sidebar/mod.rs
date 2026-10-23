//! The conversation list.

use super::*;
use crate::api::agents::{ConversationNode, get_conversation_tree};

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
    // Every conversation with its marks: a turn running (SME-41 D9), a
    // question or trust card waiting for the user (SME-34), and an agent's
    // persona and status (SME-246). Derived on the server at each fetch, so
    // a conversation, turn or question change refetches it all.
    let initial_conversations = use_resource(move || {
        let _ = conversations_changed();
        let _ = turns_changed();
        let _ = questions_changed();
        get_conversation_tree()
    });
    let mut conversations: Signal<Vec<ConversationNode>> = use_signal(Vec::new);
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
                    conversations.write().insert(0, ConversationNode::from(conversation));
                    navigator.push(Route::ConversationRoute { id });
                }
                Err(e) => action_error.set(Some(server_error_message(&e))),
            }
        });
    };

    // Opening a row, by a click or from the keyboard.
    let mut open_conversation = move |id: i64| {
        pending_delete.set(None);
        open_on_phone.set(false);
        navigator.push(Route::ConversationRoute { id });
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

    // Whether the sidebar's live connection is up, for `data-app-live`: the
    // app-wide stream while no conversation is open. With one selected, its
    // stream relays the same events, so nothing listens here (SME-269).
    #[allow(unused_mut)]
    let mut app_live: Signal<bool> = use_signal(|| false);
    #[cfg(feature = "web")]
    use_sidebar_app_events(
        selected,
        conversations_changed,
        pods_changed,
        turns_changed,
        questions_changed,
        app_live,
    );

    rsx! {
        aside { class: if open_on_phone() { "sidebar sidebar-open" } else { "sidebar" },
            // Phone only (see assets/css/01-base.css): the list folds behind this, so the
            // conversation, not the list, fills the screen (SME-41 D4).
            button {
                class: "sidebar-toggle",
                r#type: "button",
                aria_expanded: "{open_on_phone()}",
                onclick: move |_| open_on_phone.toggle(),
                if open_on_phone() { "Close" } else { "Conversations" }
            }
            div { class: "sidebar-body",
            // Set once this tab's sidebar connection is up, so a browser
            // test can tell a hydrated, listening page from a static
            // server render (SME-269).
            "data-app-live": app_live().then_some("true"),
            button { class: "new-conversation", onclick: move |e| { open_on_phone.set(false); new_conversation(e) }, "New conversation" }
            Link { to: Route::ProvidersRoute {}, class: "sandbox-volumes-link providers-link", "Model providers" }
            Link { to: Route::McpServersRoute {}, class: "mcp-servers-link", "MCP servers" }
            Link { to: Route::PodsRoute {}, class: "pods-link", "Sandboxes" }
            Link { to: Route::SandboxVolumesRoute {}, class: "sandbox-volumes-link", "Sandbox volumes" }
            Link { to: Route::GitRoute {}, class: "sandbox-volumes-link git-link", "Git" }
            Link { to: Route::LanguageServersRoute {}, class: "sandbox-volumes-link language-servers-link", "Language servers" }
            super::ErrorText { message: list_error() }
            super::ErrorText { message: action_error() }
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
                            onclick: move |_| open_conversation(conversation.id),
                            // The title is the row's link and tab stop, which
                            // Enter or Space opens (SME-58). Not the whole
                            // row: its Delete would be inside the link, read
                            // as part of its name and, by some screen
                            // readers, not reachable at all.
                            span {
                                class: "conversation-title",
                                tabindex: "0",
                                role: "link",
                                aria_current: if selected() == Some(conversation.id) { Some("page") } else { None },
                                onkeydown: move |evt: Event<KeyboardData>| {
                                    let opens = match evt.key() {
                                        keyboard_types::Key::Enter => true,
                                        keyboard_types::Key::Character(c) => c == " ",
                                        _ => false,
                                    };
                                    if opens {
                                        // Space would scroll the list too.
                                        evt.prevent_default();
                                        open_conversation(conversation.id);
                                    }
                                },
                                "{conversation.title}"
                            }
                            if conversation.busy {
                                span { class: "conversation-busy", title: "Working" }
                            }
                            if conversation.waiting_for_user {
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
                                confirm: delete_confirm_label(&conversations(), conversation.id),
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

/// The armed Delete's label: how many agents go with the conversation, its
/// whole subtree, counted from the list the sidebar already has (SME-236).
fn delete_confirm_label(conversations: &[ConversationNode], id: i64) -> String {
    let mut under = 0;
    let mut frontier = vec![id];
    while let Some(parent) = frontier.pop() {
        for child in conversations.iter().filter(|c| c.parent_id == Some(parent)) {
            under += 1;
            frontier.push(child.id);
        }
    }
    match under {
        0 => "Confirm?".to_string(),
        1 => "Delete + 1 agent?".to_string(),
        n => format!("Delete + {n} agents?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(id: i64, parent_id: Option<i64>) -> ConversationNode {
        let at = chrono::NaiveDateTime::default();
        ConversationNode::from(crate::models::Conversation { id, title: format!("c{id}"), created_at: at, updated_at: at, parent_id })
    }

    #[test]
    fn test_the_delete_label_counts_the_whole_subtree() {
        let list = vec![
            conversation(1, None),
            conversation(2, Some(1)),
            conversation(3, Some(1)),
            conversation(4, Some(2)),
            conversation(5, None),
        ];
        assert_eq!(delete_confirm_label(&list, 1), "Delete + 3 agents?");
        assert_eq!(delete_confirm_label(&list, 2), "Delete + 1 agent?");
        assert_eq!(delete_confirm_label(&list, 5), "Confirm?");
    }
}
