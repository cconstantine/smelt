//! The line over a conversation that says which one it is (SME-123 DR5):
//! its title and, for an agent, its status, persona and the conversation
//! that started it. Before, the sidebar's highlight was the only clue, and
//! on a phone, with the list folded away, there was none.

use super::*;
use crate::api::agents::ConversationNode;
use super::conversation_menu::{ConversationMenu, MenuItem, menu_items};

/// What the header shows for one conversation.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HeaderInfo {
    pub title: String,
    /// "status · persona" for an agent; None for the user's own.
    pub meta: Option<String>,
    /// The conversation that started it: its id and title.
    pub parent: Option<(i64, String)>,
    /// `Some(archived)` for a top-level conversation, which can be
    /// archived (SME-480 D6); `None` for an agent, which follows its root.
    pub archive: Option<bool>,
}

/// The header for conversation `id`, from the conversation list; None
/// while the list doesn't have it (the header is then its toggle alone).
pub(super) fn header_info(nodes: &[ConversationNode], id: i64) -> Option<HeaderInfo> {
    let node = nodes.iter().find(|n| n.id == id)?;
    let meta = node.status.as_deref().map(|status| match node.persona.as_deref() {
        Some(persona) => format!("{status} \u{b7} {persona}"),
        None => status.to_string(),
    });
    let parent = node
        .parent_id
        .and_then(|p| nodes.iter().find(|n| n.id == p))
        .map(|p| (p.id, p.title.clone()));
    let archive = node.parent_id.is_none().then_some(node.archived_at.is_some());
    Some(HeaderInfo { title: node.title.clone(), meta, parent, archive })
}

#[component]
pub(super) fn ConversationHeader(
    selected: Memo<Option<i64>>,
    nodes: Signal<Vec<ConversationNode>>,
    state: Store<ConversationState>,
) -> Element {
    // Without the tree (not fetched yet, or a row it doesn't have yet) the
    // header is the ☰ menu with Show thinking and Compact: thinking can be
    // shown from the start (SME-354 B18).
    let info = selected().and_then(|id| header_info(&nodes.read(), id));
    let listed = info.is_some();
    let (title, meta, parent, archive) = match info {
        Some(info) => (info.title, info.meta, info.parent, info.archive),
        None => (String::new(), None, None, None),
    };
    // Kept with the conversation it's about: the header isn't remounted
    // on a switch (review 1, L1), so another's error mustn't show here.
    let header_error: Signal<Option<(i64, String)>> = use_signal(|| None);
    let mut menu_open = use_signal(|| false);
    let mut compact_open = use_signal(|| false);
    let compact_busy = use_signal(|| None::<i64>);
    // Another conversation opened: its menu starts closed.
    use_effect(move || {
        selected();
        menu_open.set(false);
    });
    let compacting = super::compact_control::compacting_here(compact_busy(), selected());
    let on_action = move |item: MenuItem| {
        menu_open.set(false);
        match item {
            // Thinking beside tool calls and replies is hidden unless
            // asked for (SME-123 DR6).
            MenuItem::Thinking => {
                let shown = *super::transcript::SHOW_THINKING.peek();
                *super::transcript::SHOW_THINKING.write() = !shown;
            }
            // Today's popover, under ☰ (SME-513 Q1).
            MenuItem::Compact => compact_open.set(true),
            MenuItem::Archive => {
                if let (Some(archived), Some(id)) = (archive, selected()) {
                    archive_conversation(id, archived, nodes, header_error);
                }
            }
        }
    };
    rsx! {
        div { class: "conversation-header",
            // Names the conversation for the browser tier's diagnostics
            // (SME-505's `page_state`).
            "data-conversation-id": selected().map(|id| id.to_string()),
            div { class: "conversation-header-top",
                h1 { class: "conversation-header-title", title: "{title}", "{title}" }
                // The popover's button went into the menu: a compaction
                // left running still says so (SME-513 Q4).
                if compacting {
                    span { class: "conversation-compacting", "Compacting…" }
                }
                ConversationMenu {
                    open: menu_open,
                    items: menu_items(archive, listed),
                    archived: archive.unwrap_or(false),
                    turn_running: state.turn_running()(),
                    compacting,
                    on_action,
                }
                // Last in the row: its popover is right-aligned under ☰ and
                // 320 px wide, so anything after it pushes the popover off a
                // phone's left edge (SME-482's compact_now at 375 px).
                super::compact_control::CompactControl { selected, state, open: compact_open, busy: compact_busy }
            }
            super::ErrorText { message: header_error_for(header_error(), selected()) }
            if meta.is_some() || parent.is_some() {
                div { class: "conversation-header-meta",
                    if let Some(meta) = meta {
                        span { "{meta}" }
                    }
                    if let Some((parent_id, parent_title)) = parent {
                        span { class: "conversation-header-parent",
                            "part of "
                            Link { to: Route::ConversationRoute { id: parent_id }, "{parent_title}" }
                        }
                    }
                }
            }
        }
    }
}

/// Archive / Unarchive for a top-level conversation (SME-480 D6), from
/// the menu. The returned mark goes into the shared tree at once; the
/// refetch on `ConversationsChanged` confirms it. A failure shows under
/// the header.
fn archive_conversation(id: i64, archived: bool, nodes: Signal<Vec<ConversationNode>>, error: Signal<Option<(i64, String)>>) {
    let mut nodes = nodes;
    let mut error = error;
    spawn(async move {
        let result = if archived {
            crate::api::archive::unarchive_conversation(id).await
        } else {
            crate::api::archive::archive_conversation(id).await
        };
        match result {
            Ok(conversation) => {
                error.set(None);
                if let Some(node) = nodes.write().iter_mut().find(|n| n.id == id) {
                    node.archived_at = conversation.archived_at;
                }
            }
            Err(e) => error.set(Some((id, server_error_message(&e)))),
        }
    });
}

/// The error to show under `selected`'s header: only one from that
/// conversation's own menu actions (review 1, L1).
fn header_error_for(error: Option<(i64, String)>, selected: Option<i64>) -> Option<String> {
    error.filter(|(id, _)| selected == Some(*id)).map(|(_, message)| message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, parent_id: Option<i64>, title: &str, status: Option<&str>, persona: Option<&str>) -> ConversationNode {
        let at = chrono::NaiveDateTime::default();
        ConversationNode {
            id,
            title: title.to_string(),
            created_at: at,
            updated_at: at,
            parent_id,
            persona: persona.map(str::to_string),
            status: status.map(str::to_string),
            busy: false,
            waiting_for_user: false,
            report_owed_at: None,
            archived_at: None,
        }
    }

    fn rendered_header(nodes: Vec<ConversationNode>, id: i64) -> String {
        fn app(props: (Vec<ConversationNode>, i64)) -> Element {
            let (list, id) = props;
            let selected = use_memo(move || Some(id));
            let nodes = use_signal(move || list.clone());
            let state = use_store(ConversationState::default);
            rsx! { ConversationHeader { selected, nodes, state } }
        }
        let mut dom = VirtualDom::new_with_props(app, (nodes, id));
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    /// SME-354 B18: "Show thinking" lived only in a header that rendered
    /// nothing until the conversation tree loaded. With the tree not in
    /// yet (or without the open conversation), the ☰ menu that holds it
    /// still shows (SME-513 S1).
    #[cfg(feature = "server")]
    #[test]
    fn test_the_menu_shows_before_the_tree_has_the_conversation() {
        let html = rendered_header(Vec::new(), 7);
        assert!(html.contains("conversation-menu-button"), "{html}");
        assert!(html.contains(r#"data-conversation-id="7""#), "{html}");
    }

    /// SME-513 S2 and S3: a listed conversation's header holds the menu
    /// too, and none of the old row of buttons.
    #[cfg(feature = "server")]
    #[test]
    fn test_a_listed_conversations_header_holds_the_menu_alone() {
        let nodes = vec![node(1, None, "root", None, None), node(2, Some(1), "seat", Some("idle"), None)];
        for id in [1, 2] {
            let html = rendered_header(nodes.clone(), id);
            assert!(html.contains("conversation-menu-button"), "{id}: {html}");
            for gone in ["conversation-thinking-toggle", "conversation-archive", "conversation-compact-toggle"] {
                assert!(!html.contains(gone), "{id}: {gone} in {html}");
            }
        }
    }

    #[test]
    fn test_an_agents_header_names_its_status_persona_and_parent() {
        let nodes = [
            node(1, None, "Ticket Driver for SME-345", None, None),
            node(2, Some(1), "Developer seat on SME-345", Some("finished"), Some("smelt:developer")),
        ];
        assert_eq!(
            header_info(&nodes, 2),
            Some(HeaderInfo {
                title: "Developer seat on SME-345".to_string(),
                meta: Some("finished \u{b7} smelt:developer".to_string()),
                parent: Some((1, "Ticket Driver for SME-345".to_string())),
                archive: None,
            })
        );
        assert_eq!(
            header_info(&nodes, 1),
            Some(HeaderInfo { title: "Ticket Driver for SME-345".to_string(), meta: None, parent: None, archive: Some(false) }),
            "the user's own: its title alone"
        );
        assert_eq!(header_info(&nodes, 3), None);
    }

    /// Review 1, L1: a failed Archive on one conversation isn't shown
    /// under the next one's header, and shows again on going back.
    #[test]
    fn test_a_header_error_shows_only_under_its_own_conversation() {
        let error = Some((1, "conversation not found".to_string()));
        assert_eq!(header_error_for(error.clone(), Some(1)).as_deref(), Some("conversation not found"));
        assert_eq!(header_error_for(error.clone(), Some(2)), None, "another conversation's header");
        assert_eq!(header_error_for(error, None), None, "no conversation open");
    }

    /// SME-480 D6: a top-level conversation offers Archive or Unarchive by
    /// its own mark; an agent offers neither, even in an archived tree.
    #[test]
    fn test_only_a_top_level_conversation_offers_archive_by_its_mark() {
        let mut archived = node(1, None, "archived root", None, None);
        archived.archived_at = Some(chrono::NaiveDateTime::default());
        let mut agent = node(2, Some(1), "its agent", Some("idle"), None);
        agent.archived_at = archived.archived_at;
        let nodes = [archived, agent, node(3, None, "in use", None, None)];
        let archive = |id| header_info(&nodes, id).expect("listed").archive;
        assert_eq!(archive(1), Some(true));
        assert_eq!(archive(3), Some(false));
        assert_eq!(archive(2), None);
    }
}
