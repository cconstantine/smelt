//! The line over a conversation that says which one it is (SME-123 DR5):
//! its title and, for an agent, its status, persona and the conversation
//! that started it. Before, the sidebar's highlight was the only clue, and
//! on a phone, with the list folded away, there was none.

use super::*;
use crate::api::agents::{ConversationNode, get_conversation_tree};

/// What the header shows for one conversation.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HeaderInfo {
    pub title: String,
    /// "status · persona" for an agent; None for the user's own.
    pub meta: Option<String>,
    /// The conversation that started it: its id and title.
    pub parent: Option<(i64, String)>,
}

/// The header for conversation `id`, from the conversation list; None
/// while the list doesn't have it.
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
    Some(HeaderInfo { title: node.title.clone(), meta, parent })
}

#[component]
pub(super) fn ConversationHeader(
    selected: Memo<Option<i64>>,
    conversations_changed: Signal<u64>,
    turns_changed: Signal<u64>,
    questions_changed: Signal<u64>,
) -> Element {
    let tree = use_resource(move || {
        let _ = conversations_changed();
        let _ = turns_changed();
        // A question waiting changes an agent's status (review 1 L8).
        let _ = questions_changed();
        get_conversation_tree()
    });
    let info = match (&*tree.read(), selected()) {
        (Some(Ok(nodes)), Some(id)) => header_info(nodes, id),
        _ => None,
    };
    let Some(info) = info else {
        return rsx! {};
    };
    rsx! {
        div { class: "conversation-header",
            div { class: "conversation-header-top",
                h1 { class: "conversation-header-title", title: "{info.title}", "{info.title}" }
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
            }
            if info.meta.is_some() || info.parent.is_some() {
                div { class: "conversation-header-meta",
                    if let Some(meta) = info.meta {
                        span { "{meta}" }
                    }
                    if let Some((parent_id, parent_title)) = info.parent {
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
            })
        );
        assert_eq!(
            header_info(&nodes, 1),
            Some(HeaderInfo { title: "Ticket Driver for SME-345".to_string(), meta: None, parent: None }),
            "the user's own: its title alone"
        );
        assert_eq!(header_info(&nodes, 3), None);
    }
}
