//! The conversation list.

use super::*;
use std::collections::HashSet;

use crate::api::agents::{ConversationNode, get_conversation_tree};

mod tree;

use tree::{TreeRow, build_tree, unfold_to};

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

/// Which conversations' agents are folded away. App-wide, not the
/// sidebar's own state: Home and a conversation's page each mount their own
/// `Chat`, so a sidebar signal reset on every move between them (SME-246
/// review 1). Per tab, like any signal; a reload unfolds everything.
static FOLDED: GlobalSignal<HashSet<i64>> = Signal::global(HashSet::new);

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
    // The conversations whose agents are folded away, in this tab only: a
    // reload shows every agent again (SME-246).
    let mut collapsed: Signal<HashSet<i64>> = FOLDED.signal();
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

    // Opening an agent inside a folded parent (its banner link, a URL)
    // unfolds the parents above it, once per opening: the list refetches on
    // every turn anywhere, and a fold the user makes after opening must hold
    // (SME-246 review 2). It waits for the list to have the open row.
    let mut unfolded_for: Signal<Option<i64>> = use_signal(|| None);
    use_effect(move || {
        let open = selected();
        let nodes = conversations();
        if *unfolded_for.peek() == open || !open.is_some_and(|id| nodes.iter().any(|n| n.id == id)) {
            return;
        }
        unfolded_for.set(open);
        let mut folded = collapsed.peek().clone();
        if unfold_to(&mut folded, &nodes, open) {
            collapsed.set(folded);
        }
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
                // The folded list still says something waits for the user.
                {waiting_count(waiting_total(&conversations()), |n| match n {
                    1 => "1 conversation is waiting for you".to_string(),
                    n => format!("{n} conversations are waiting for you"),
                })}
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
            Link { to: Route::SecretsRoute {}, class: "sandbox-volumes-link secrets-link", "Secrets" }
            Link { to: Route::LanguageServersRoute {}, class: "sandbox-volumes-link language-servers-link", "Language servers" }
            super::ErrorText { message: list_error() }
            super::ErrorText { message: action_error() }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else if conversations().is_empty() {
                p { class: "muted", "No conversations yet" }
            } else {
                div { class: "conversation-list",
                    for row in build_tree(&conversations(), &collapsed()) {
                        ConversationRow {
                            key: "{row.node.id}",
                            active: selected() == Some(row.node.id),
                            live_pod: has_live_pod(row.node.id),
                            age: now_utc().map(|now| short_age((now - row.node.updated_at).num_seconds())),
                            armed: pending_delete() == Some(row.node.id),
                            confirm: delete_confirm_label(&conversations(), row.node.id),
                            on_open: move |id| open_conversation(id),
                            on_delete: move |id| request_delete(id),
                            on_toggle: move |id| {
                                let mut folded = collapsed.write();
                                if !folded.remove(&id) {
                                    folded.insert(id);
                                }
                            },
                            row,
                        }
                    }
                }
            }
            }
        }
    }
}

/// One row of the list: a conversation, indented under its parent, with
/// its marks and, for an agent, its persona and status (SME-246).
#[component]
fn ConversationRow(
    row: TreeRow,
    active: bool,
    live_pod: bool,
    age: Option<String>,
    armed: bool,
    confirm: String,
    on_open: EventHandler<i64>,
    on_delete: EventHandler<i64>,
    on_toggle: EventHandler<i64>,
) -> Element {
    let id = row.node.id;
    let class = match (active, row.node.status.is_some()) {
        (true, true) => "conversation-item conversation-agent active",
        (true, false) => "conversation-item active",
        (false, true) => "conversation-item conversation-agent",
        (false, false) => "conversation-item",
    };
    rsx! {
        div {
            "data-conversation-id": "{id}",
            "data-depth": "{row.depth}",
            class,
            style: "--depth: {row.indent()}",
            onclick: move |_| on_open.call(id),
            if row.has_children {
                button {
                    class: "tree-caret",
                    r#type: "button",
                    aria_expanded: "{!row.collapsed}",
                    aria_label: if row.collapsed { "Show its agents" } else { "Hide its agents" },
                    title: if row.collapsed { "Show its agents" } else { "Hide its agents" },
                    onclick: move |evt: Event<MouseData>| {
                        // Folding isn't opening the conversation.
                        evt.stop_propagation();
                        on_toggle.call(id);
                    },
                    if row.collapsed { "\u{25b8}" } else { "\u{25be}" }
                }
            } else if row.depth > 0 {
                // The caret's room, so an agent's title lines up with its
                // siblings' that have agents of their own.
                span { class: "tree-caret-spacer" }
            }
            // The title is the row's link and tab stop, which Enter or
            // Space opens (SME-58). Not the whole row: its Delete would be
            // inside the link, read as part of its name and, by some screen
            // readers, not reachable at all.
            span {
                class: "conversation-title",
                tabindex: "0",
                role: "link",
                aria_current: if active { Some("page") } else { None },
                onkeydown: move |evt: Event<KeyboardData>| {
                    let opens = match evt.key() {
                        keyboard_types::Key::Enter => true,
                        keyboard_types::Key::Character(c) => c == " ",
                        _ => false,
                    };
                    if opens {
                        // Space would scroll the list too.
                        evt.prevent_default();
                        on_open.call(id);
                    }
                },
                "{row.node.title}"
            }
            if row.node.busy {
                span { class: "conversation-busy", title: "Working" }
            }
            if row.node.waiting_for_user {
                span { class: "conversation-waiting", title: "Waiting for your answer", "?" }
            }
            {waiting_count(row.waiting_under, |n| match n {
                1 => "1 agent under it is waiting for you".to_string(),
                n => format!("{n} agents under it are waiting for you"),
            })}
            if live_pod {
                span { class: "live-pod-dot", title: "sandbox pod running" }
            }
            if let Some(age) = age {
                span { class: "conversation-age", "{age}" }
            }
            super::TwoStepButton {
                armed,
                class: "delete-conversation",
                idle: "Delete",
                confirm,
                on_arm: move |_| on_delete.call(id),
                on_confirm: move |_| on_delete.call(id),
            }
            {agent_meta(&row.node)}
        }
    }
}

/// How many conversations in the list wait for the user.
fn waiting_total(conversations: &[ConversationNode]) -> usize {
    conversations.iter().filter(|c| c.waiting_for_user).count()
}

/// The mark that finds a waiting agent from further up: on a parent's row
/// (its agents, at any depth, folded or not) and on the phone's folded
/// list, `label` saying what the count is. Nothing when none waits.
fn waiting_count(count: usize, label: impl Fn(usize) -> String) -> Element {
    if count == 0 {
        return rsx! {};
    }
    let label = label(count);
    rsx! {
        span { class: "agents-waiting-count", title: "{label}", aria_label: "{label}", "?{count}" }
    }
}

/// An agent's second line: its status, then its persona, which gives way
/// first on a narrow row. Nothing for the user's own conversations.
fn agent_meta(node: &ConversationNode) -> Element {
    let Some(status) = node.status.as_deref() else {
        return rsx! {};
    };
    let status_class = format!("agent-status status-{}", status.replace(' ', "-"));
    rsx! {
        div { class: "conversation-agent-meta",
            span { class: status_class, "{status}" }
            if let Some(persona) = node.persona.as_deref() {
                span { class: "agent-meta-sep", aria_hidden: "true", " \u{b7} " }
                span { class: "agent-persona", "{persona}" }
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

    fn agent(id: i64, parent_id: i64, persona: Option<&str>, status: &str) -> ConversationNode {
        let mut node = conversation(id, Some(parent_id));
        node.persona = persona.map(str::to_string);
        node.status = Some(status.to_string());
        node
    }

    #[test]
    fn test_an_agents_meta_line_shows_its_status_then_its_persona() {
        let html = dioxus::ssr::render_element(agent_meta(&agent(2, 1, Some("smelt:developer"), "waiting for the user")));
        assert!(html.contains(r#"class="conversation-agent-meta""#), "{html}");
        assert!(html.contains(">smelt:developer<"), "{html}");
        assert!(html.contains(r#"class="agent-status status-waiting-for-the-user">waiting for the user<"#), "{html}");
        assert!(html.find("waiting for the user") < html.find("smelt:developer"), "the status comes first: {html}");
    }

    #[test]
    fn test_an_agent_without_a_persona_shows_its_status_alone() {
        let html = dioxus::ssr::render_element(agent_meta(&agent(2, 1, None, "idle")));
        assert!(html.contains(">idle<"), "{html}");
        assert!(!html.contains("agent-persona") && !html.contains("agent-meta-sep"), "{html}");
    }

    #[test]
    fn test_the_users_own_conversation_has_no_meta_line() {
        assert_eq!(dioxus::ssr::render_element(agent_meta(&conversation(1, None))), "");
    }

    #[test]
    fn test_the_waiting_count_names_how_many_and_is_absent_at_zero() {
        let label = |n: usize| format!("{n} waiting");
        assert_eq!(dioxus::ssr::render_element(waiting_count(0, label)), "");
        let two = dioxus::ssr::render_element(waiting_count(2, label));
        assert!(two.contains(r#"title="2 waiting""#) && two.contains(r#"aria-label="2 waiting""#), "{two}");
        assert!(two.contains(">?2<"), "{two}");
    }

    #[test]
    fn test_the_waiting_total_counts_every_conversation_waiting() {
        let mut list = vec![conversation(1, None), agent(2, 1, None, "waiting for the user"), conversation(3, None)];
        list[1].waiting_for_user = true;
        list[2].waiting_for_user = true;
        assert_eq!(waiting_total(&list), 2);
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
