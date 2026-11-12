//! The conversation list.

use super::*;
use std::collections::HashSet;

use crate::api::agents::{ConversationNode, get_conversation_tree};
use crate::frontend::notifications::NotificationsToggle;

mod archive;
mod tree;

use tree::{TreeRow, build_tree, settled_parents, unfold_to};

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
/// The parents the sidebar has folded by itself, or the user has folded or
/// unfolded: each folds on its own once at most, so a tree the user opened
/// stays open (SME-123 DR1).
static FOLD_DECIDED: GlobalSignal<HashSet<i64>> = Signal::global(HashSet::new);
/// Whether archived trees are listed (SME-480 D8): per tab, like `FOLDED`,
/// and off on every load.
static SHOW_ARCHIVED: GlobalSignal<bool> = Signal::global(|| false);

/// "Show archived (N)" / "Hide archived" over the list, when `count`
/// top-level conversations are archived; nothing at zero.
#[component]
fn ShowArchivedToggle(count: usize) -> Element {
    if count == 0 {
        return rsx! {};
    }
    let shown = SHOW_ARCHIVED();
    rsx! {
        button {
            class: "show-archived",
            r#type: "button",
            aria_pressed: "{shown}",
            onclick: move |_| {
                let now = *SHOW_ARCHIVED.peek();
                *SHOW_ARCHIVED.write() = !now;
            },
            if shown { "Hide archived" } else { "Show archived ({count})" }
        }
    }
}

#[component]
pub(super) fn ConversationSidebar(
    selected: Memo<Option<i64>>,
    /// The tree as last fetched, shared with the conversation header.
    conversations: Signal<Vec<ConversationNode>>,
    conversations_changed: Signal<u64>,
    pods_changed: Signal<u64>,
    turns_changed: Signal<u64>,
    questions_changed: Signal<u64>,
    commands_changed: Signal<u64>,
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
    // Which conversations have running terminal commands on a live pod,
    // and how many — the amber marker beside the pod dot (SME-441). A
    // failed fetch means no markers and no error line, as for the dot.
    let live_commands = use_resource(move || {
        let _ = commands_changed();
        // …and PodsChanged too (review round 1): a user Stop closes the
        // pod row and publishes PodsChanged after that write lands, so
        // this refetch sees the live-pod join already exclude the pod —
        // the marker never outlives the green dot by a round trip, and a
        // failed lost-marking can't strand it either. CommandsChanged
        // still covers every other flip.
        let _ = pods_changed();
        crate::api::pods::get_running_command_conversations()
    });
    let running_commands_for = move |id: i64| -> i64 {
        match &*live_commands.read() {
            Some(Ok(rows)) => rows
                .iter()
                .find(|row| row.conversation_id == id)
                .map(|row| row.running_commands)
                .unwrap_or(0),
            _ => 0,
        }
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
    let mut conversations = conversations;
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
                    fold_settled_trees(&list, *selected.peek());
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
        commands_changed,
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
            // Settings, as one compact block: seven full-width buttons took
            // about 400px above the conversations (SME-123 DR4).
            nav { class: "settings-links", aria_label: "Settings",
                Link { to: Route::ProvidersRoute {}, class: "settings-link providers-link", "Model providers" }
                Link { to: Route::McpServersRoute {}, class: "settings-link mcp-servers-link", "MCP servers" }
                Link { to: Route::PodsRoute {}, class: "settings-link pods-link", "Sandboxes" }
                Link { to: Route::SandboxVolumesRoute {}, class: "settings-link sandbox-volumes-link", "Sandbox volumes" }
                Link { to: Route::GitRoute {}, class: "settings-link git-link", "Git" }
                Link { to: Route::SecretsRoute {}, class: "settings-link secrets-link", "Secrets" }
                Link { to: Route::LanguageServersRoute {}, class: "settings-link language-servers-link", "Language servers" }
                // The one place the browser's notification permission
                // prompt ever appears (SME-71).
                NotificationsToggle {}
            }
            super::ErrorText { message: list_error() }
            super::ErrorText { message: action_error() }
            // SME-480 D8: shown only when there is something archived.
            ShowArchivedToggle { count: archive::archived_roots(&conversations()) }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else if conversations().is_empty() {
                p { class: "muted", "No conversations yet" }
            } else {
                div { class: "conversation-list",
                    // Archived trees drop out here, unless shown, owed
                    // something (D1) or open (D7); the phone's count above
                    // reads the whole list.
                    for row in build_tree(&archive::visible(&conversations(), selected(), SHOW_ARCHIVED()), &collapsed()) {
                        ConversationRow {
                            key: "{row.node.id}",
                            active: selected() == Some(row.node.id),
                            live_pod: has_live_pod(row.node.id),
                            running_commands: running_commands_for(row.node.id),
                            age: now_utc().map(|now| short_age((now - row.node.updated_at).num_seconds())),
                            report_owed: report_marker(row.node.report_owed_at, row.node.busy, now_utc()),
                            armed: pending_delete() == Some(row.node.id),
                            confirm: delete_confirm_label(&conversations(), row.node.id),
                            on_open: move |id| open_conversation(id),
                            on_delete: move |id| request_delete(id),
                            on_toggle: move |id| {
                                FOLD_DECIDED.write().insert(id);
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

/// Folds each settled agent tree the first time the list shows it settled
/// (`tree::settled_parents`), except above the open conversation: its
/// row has to show.
fn fold_settled_trees(nodes: &[ConversationNode], open: Option<i64>) {
    // The open conversation and its ancestors stay open: folding the tree
    // the user is looking at, on a refetch, hid its rows (review 1 L2).
    let mut keep_open: HashSet<i64> = open.into_iter().collect();
    let parents: std::collections::HashMap<i64, Option<i64>> = nodes.iter().map(|n| (n.id, n.parent_id)).collect();
    let mut at = open.and_then(|id| parents.get(&id).copied().flatten());
    while let Some(id) = at {
        if !keep_open.insert(id) {
            break;
        }
        at = parents.get(&id).copied().flatten();
    }
    let undecided: Vec<i64> = settled_parents(nodes)
        .into_iter()
        .filter(|id| !FOLD_DECIDED.peek().contains(id))
        .collect();
    if undecided.is_empty() {
        return;
    }
    // A tree the user has seen open stays open: deciding it here keeps it
    // from folding later, once they've moved on (review 1 L2).
    FOLD_DECIDED.write().extend(undecided.iter().copied());
    FOLDED.write().extend(undecided.into_iter().filter(|id| !keep_open.contains(id)));
}

/// One row of the list: a conversation, indented under its parent, with
/// its marks and, for an agent, its persona and status (SME-246).
#[component]
fn ConversationRow(
    row: TreeRow,
    active: bool,
    live_pod: bool,
    running_commands: i64,
    age: Option<String>,
    report_owed: Option<String>,
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
            // Each mark says what it means, to a pointer and a screen
            // reader (SME-123 DR14).
            if row.node.busy {
                span { class: "conversation-busy", role: "img", title: "Working", aria_label: "Working" }
            }
            if row.node.waiting_for_user {
                span { class: "conversation-waiting", title: "Waiting for your answer", "?" }
            }
            {waiting_count(row.waiting_under, |n| match n {
                1 => "1 agent under it is waiting for you".to_string(),
                n => format!("{n} agents under it are waiting for you"),
            })}
            if live_pod {
                span { class: "live-pod-dot", role: "img", title: "Sandbox running", aria_label: "Sandbox running" }
            }
            // An idle conversation whose sandbox still runs a command
            // pulses amber beside the green dot (SME-441); while a turn
            // runs, the busy spinner above wins and this stays hidden.
            if let Some(title) = command_marker(running_commands, row.node.busy) {
                span { class: "live-command-dot", role: "img", title: "{title}", aria_label: "{title}" }
            }
            // A live seat that ended its turn owing its parent a report
            // with nothing running (SME-455). The title is fixed words
            // plus the age only — no model words, no command text
            // (the SME-441 quoting rule); it reads the gated tree column.
            if let Some(title) = report_owed {
                span { class: "conversation-report-owed", role: "img", title: "{title}", aria_label: "{title}", "!" }
            }
            // SME-480: an archived tree shown (by the toggle, D1 or D7)
            // says so on its root's row only.
            if row.depth == 0 && row.node.archived_at.is_some() {
                span { class: "conversation-archived", "archived" }
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
            {tree_summary(&row)}
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
/// first on a narrow row. Nothing for the user's own conversations, except
/// one set to a persona (SME-481): the persona alone, with no status.
fn agent_meta(node: &ConversationNode) -> Element {
    let Some(status) = node.status.as_deref() else {
        return match node.persona.as_deref() {
            Some(persona) => rsx! {
                div { class: "conversation-agent-meta",
                    span { class: "agent-persona", "{persona}" }
                }
            },
            None => rsx! {},
        };
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

/// A parent conversation's second line, when it isn't an agent itself:
/// how many agents it has and how many are working (SME-123 DR1: a root
/// row said nothing of the tree under it).
fn tree_summary(row: &TreeRow) -> Element {
    if row.node.status.is_some() || row.agents_under == 0 {
        return rsx! {};
    }
    let agents = match row.agents_under {
        1 => "1 agent".to_string(),
        n => format!("{n} agents"),
    };
    rsx! {
        div { class: "conversation-agent-meta conversation-tree-summary",
            span { "{agents}" }
            if row.busy_under > 0 {
                span { class: "agent-meta-sep", aria_hidden: "true", " \u{b7} " }
                span { class: "agent-status status-running", "{row.busy_under} working" }
            }
        }
    }
}

/// The sidebar's command marker's title: `Some` when the conversation
/// isn't running a turn (the busy mark wins — Decided (SME-441)) and has
/// running commands on its live pod. The count is the whole title: no
/// command text reaches the attribute, whatever the command is running.
pub(super) fn command_marker(running_commands: i64, busy: bool) -> Option<String> {
    match (running_commands, busy) {
        (0, _) | (_, true) => None,
        (1, _) => Some("1 command running".into()),
        (n, _) => Some(format!("{n} commands running")),
    }
}

/// The sidebar's report-owed marker's title (SME-455): `Some` when the
/// seat owes its parent a report (the gated `report_owed_at` the tree
/// carries) and isn't running a turn — the busy mark wins, as with the
/// command marker — and the browser clock is ready. The age is the owed
/// moment's, so it survives the predicate flickering; under a minute
/// `short_age` still says `now`, and there's no age to tell yet.
pub(super) fn report_marker(owed_at: Option<chrono::NaiveDateTime>, busy: bool, now: Option<chrono::NaiveDateTime>) -> Option<String> {
    match (owed_at, busy, now) {
        (Some(owed_at), false, Some(now)) => {
            let age = short_age((now - owed_at).num_seconds());
            Some(if age == "now" {
                "owes its parent a report".into()
            } else {
                format!("owes its parent a report for {age}")
            })
        }
        _ => None,
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
mod tests;
