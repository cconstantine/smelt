//! Compact's popover (SME-482), opened from the header's ☰ menu (SME-513):
//! compacts the conversation
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

/// Whether the open conversation's own compaction is in flight: `busy` is
/// the conversation a compaction was asked for, if one is (round 1 L3).
pub(super) fn compacting_here(busy: Option<i64>, selected: Option<i64>) -> bool {
    busy.is_some() && busy == selected
}

/// Compact's popover, opened from the header's ☰ menu (SME-513 Q1): the
/// header owns `open` and `busy`, so the menu's item opens it and reads
/// whether this conversation is compacting.
#[component]
pub(super) fn CompactControl(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    open: Signal<bool>,
    busy: Signal<Option<i64>>,
) -> Element {
    let turn_running = state.turn_running();
    let mut context_usage = state.context_usage();
    let mut open = open;
    let mut note = use_signal(String::new);
    let mut busy = busy;
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
        busy.set(Some(id));
        error.set(None);
        let sent = compact_note(&note.peek());
        spawn(async move {
            let result = compact_conversation_now(id, sent).await;
            // Only this request's own mark: a later click elsewhere set its own.
            if *busy.peek() == Some(id) {
                busy.set(None);
            }
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
    let busy_here = compacting_here(busy(), selected());
    rsx! {
        div { class: "compact-control",
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
                        // Opened from the menu: focus moves into the popover.
                        onmounted: move |e: MountedEvent| async move {
                            let _ = e.set_focus(true).await;
                        },
                    }
                    if let Some(message) = error() {
                        p { class: "compact-error", "{message}" }
                    }
                    div { class: "compact-actions",
                        button {
                            class: "compact-now",
                            r#type: "button",
                            disabled: busy_here || running,
                            onclick: move |_| compact(),
                            if busy_here { "Compacting…" } else { "Compact now" }
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

    /// Round 1 L3: a compaction in flight in one conversation leaves
    /// another's button alone, after a switch.
    #[test]
    fn test_only_the_compacting_conversation_reads_compacting() {
        assert!(compacting_here(Some(7), Some(7)));
        assert!(!compacting_here(Some(7), Some(8)), "another conversation is open");
        assert!(!compacting_here(None, Some(7)));
        assert!(!compacting_here(Some(7), None));
    }

    fn rendered(open: bool, turn_running: bool) -> String {
        fn app((is_open, turn_running): (bool, bool)) -> Element {
            let selected = use_memo(|| Some(7));
            let state = use_store(move || ConversationState { turn_running, ..ConversationState::default() });
            let open = use_signal(|| is_open);
            let busy = use_signal(|| None::<i64>);
            rsx! { CompactControl { selected, state, open, busy } }
        }
        let mut dom = VirtualDom::new_with_props(app, (open, turn_running));
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    /// T18 (SME-513): the popover's Compact now is disabled while a turn
    /// runs (the server refuses then anyway); closed, nothing shows.
    #[cfg(feature = "server")]
    #[test]
    fn test_compact_now_waits_for_the_turn_to_end() {
        let closed = rendered(false, false);
        assert!(!closed.contains("compact-popover"), "{closed}");
        let idle = rendered(true, false);
        assert!(idle.contains(">Compact now<"), "{idle}");
        assert!(!idle.contains("disabled"), "{idle}");
        let running = rendered(true, true);
        assert!(running.contains(">Compact now<"), "{running}");
        assert!(running.contains("disabled"), "{running}");
    }
}
