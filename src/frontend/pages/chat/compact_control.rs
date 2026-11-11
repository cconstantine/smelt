//! The chat header's Compact button (SME-482): compacts the conversation
//! now, between turns, with an optional note of what the summary must keep
//! (`POST /api/conversations/{id}/compact`). The divider arrives through
//! `MessagesAppended` as for every compaction; this tab refetches the
//! context meter, which no event moves (the usage is cleared, not new).

use super::*;
use crate::api::compaction::compact_conversation_now;

/// The note as the request sends it: trimmed, and none when blank.
pub(super) fn compact_note(text: &str) -> Option<String> {
    let note = text.trim();
    (!note.is_empty()).then(|| note.to_string())
}

#[component]
pub(super) fn CompactControl(selected: Memo<Option<i64>>, state: Store<ConversationState>) -> Element {
    let turn_running = state.turn_running();
    let mut context_usage = state.context_usage();
    let mut open = use_signal(|| false);
    let mut note = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    // Another conversation opened: its own popover starts closed.
    use_effect(move || {
        selected();
        open.set(false);
        error.set(None);
        note.set(String::new());
    });
    let mut compact = move || {
        let Some(id) = selected() else { return };
        busy.set(true);
        error.set(None);
        let sent = compact_note(&note.peek());
        spawn(async move {
            let result = compact_conversation_now(id, sent).await;
            busy.set(false);
            // Not onto another conversation's header (SME-51 B11).
            if selected() != Some(id) {
                return;
            }
            match result {
                Ok(()) => {
                    open.set(false);
                    note.set(String::new());
                    if let Ok(snapshot) = crate::api::chat::get_context_usage(id).await
                        && selected() == Some(id)
                    {
                        context_usage.set(Some(snapshot));
                    }
                }
                Err(e) => error.set(Some(server_error_message(&e))),
            }
        });
    };
    let running = turn_running();
    let label = if busy() { "Compacting…" } else { "Compact" };
    rsx! {
        div { class: "compact-control",
            button {
                class: "conversation-thinking-toggle conversation-compact-toggle",
                r#type: "button",
                disabled: running || busy(),
                title: if running { "Compact once the turn ends" } else { "Summarise this conversation's history now, to save tokens" },
                aria_expanded: "{open()}",
                onclick: move |_| open.set(!open()),
                "{label}"
            }
            if open() {
                div { class: "compact-backdrop", onclick: move |_| open.set(false) }
                div {
                    class: "compact-popover",
                    role: "dialog",
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() == Key::Escape {
                            open.set(false);
                        }
                    },
                    textarea {
                        class: "compact-note",
                        placeholder: "What should the summary keep? (optional)",
                        value: "{note}",
                        oninput: move |e| note.set(e.value()),
                    }
                    if let Some(message) = error() {
                        p { class: "compact-error", "{message}" }
                    }
                    div { class: "compact-actions",
                        button {
                            class: "compact-now",
                            r#type: "button",
                            disabled: busy() || running,
                            onclick: move |_| compact(),
                            if busy() { "Compacting…" } else { "Compact now" }
                        }
                        button { r#type: "button", onclick: move |_| open.set(false), "Cancel" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T18: the note is sent trimmed, and a blank one as none.
    #[test]
    fn test_the_note_is_trimmed_and_blank_is_none() {
        assert_eq!(compact_note("  keep the SHA \n"), Some("keep the SHA".to_string()));
        assert_eq!(compact_note("line one\nline two"), Some("line one\nline two".to_string()));
        assert_eq!(compact_note(" \n\t "), None);
    }

    fn rendered(turn_running: bool) -> String {
        fn app(turn_running: bool) -> Element {
            let selected = use_memo(|| Some(7));
            let state = use_store(move || ConversationState { turn_running, ..ConversationState::default() });
            rsx! { CompactControl { selected, state } }
        }
        let mut dom = VirtualDom::new_with_props(app, turn_running);
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    /// T18: the button shows, and is disabled with its reason while a turn
    /// runs (the server refuses then anyway).
    #[cfg(feature = "server")]
    #[test]
    fn test_the_compact_button_waits_for_the_turn_to_end() {
        let idle = rendered(false);
        assert!(idle.contains(">Compact<"), "{idle}");
        assert!(!idle.contains("disabled"), "{idle}");
        let running = rendered(true);
        assert!(running.contains(">Compact<"), "{running}");
        assert!(running.contains("disabled"), "{running}");
        assert!(running.contains("Compact once the turn ends"), "{running}");
    }
}
