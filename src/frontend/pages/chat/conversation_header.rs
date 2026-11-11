//! The line over a conversation that says which one it is (SME-123 DR5):
//! its title and, for an agent, its status, persona and the conversation
//! that started it. Before, the sidebar's highlight was the only clue, and
//! on a phone, with the list folded away, there was none.

use super::*;
use crate::api::agents::ConversationNode;

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
) -> Element {
    // Without the tree (not fetched yet, or a row it doesn't have yet) the
    // header is the toggle alone: thinking can be shown from the start
    // (SME-354 B18).
    let info = selected().and_then(|id| header_info(&nodes.read(), id));
    let (title, meta, parent, archive) = match info {
        Some(info) => (info.title, info.meta, info.parent, info.archive),
        None => (String::new(), None, None, None),
    };
    let archive_error: Signal<Option<String>> = use_signal(|| None);
    rsx! {
        div { class: "conversation-header",
            div { class: "conversation-header-top",
                h1 { class: "conversation-header-title", title: "{title}", "{title}" }
                // Thinking beside tool calls and replies is hidden unless
                // asked for (SME-123 DR6).
                button {
                    class: "conversation-thinking-toggle",
                    r#type: "button",
                    aria_pressed: "{super::transcript::SHOW_THINKING()}",
                    onclick: move |_| {
                        let shown = *super::transcript::SHOW_THINKING.peek();
                        *super::transcript::SHOW_THINKING.write() = !shown;
                    },
                    if super::transcript::SHOW_THINKING() { "Hide thinking" } else { "Show thinking" }
                }
                if let (Some(archived), Some(id)) = (archive, selected()) {
                    ArchiveButton { id, archived, nodes, error: archive_error }
                }
            }
            super::ErrorText { message: archive_error() }
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

/// Archive / Unarchive for a top-level conversation (SME-480 D6). The
/// returned mark goes into the shared tree at once; the refetch on
/// `ConversationsChanged` confirms it. A failure shows under the header.
#[component]
fn ArchiveButton(id: i64, archived: bool, nodes: Signal<Vec<ConversationNode>>, error: Signal<Option<String>>) -> Element {
    let mut nodes = nodes;
    let mut error = error;
    rsx! {
        button {
            class: "conversation-archive",
            r#type: "button",
            onclick: move |_| {
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
                        Err(e) => error.set(Some(server_error_message(&e))),
                    }
                });
            },
            if archived { "Unarchive" } else { "Archive" }
        }
    }
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

    /// SME-354 B18: "Show thinking" lived only in a header that rendered
    /// nothing until the conversation tree loaded. With the tree not in
    /// yet (or without the open conversation), the toggle still shows.
    #[cfg(feature = "server")]
    #[test]
    fn test_the_thinking_toggle_shows_before_the_tree_has_the_conversation() {
        fn app() -> Element {
            let selected = use_memo(|| Some(7));
            let nodes = use_signal(Vec::new);
            rsx! { ConversationHeader { selected, nodes } }
        }
        let mut dom = VirtualDom::new(app);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);
        assert!(html.contains("Show thinking"), "{html}");
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
