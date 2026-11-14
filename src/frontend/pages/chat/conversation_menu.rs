//! The conversation header's ☰ menu (SME-513): the conversation's
//! actions behind one button rather than a row of them. After the APG
//! menu button pattern: the button (`aria-haspopup`, `aria-expanded`)
//! opens a `role=menu` with focus on an item; the arrows, Home and End
//! move inside it; Escape closes it back onto the button, Tab closes it
//! and moves on, and a click outside (its backdrop) closes it.

use super::*;

/// One of the menu's actions, in the order the menu lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuItem {
    Thinking,
    Compact,
    Archive,
}

/// The items a header shows (SME-354 B18's shapes): Show thinking and
/// Compact always, even before the tree has the row (S1); Archive for a
/// listed top-level conversation (S2), never an agent (S3).
pub(super) fn menu_items(archive: Option<bool>, listed: bool) -> Vec<MenuItem> {
    let mut items = vec![MenuItem::Thinking, MenuItem::Compact];
    if listed && archive.is_some() {
        items.push(MenuItem::Archive);
    }
    items
}

/// How one item reads: its label, a second line, whether it's checked
/// (Show thinking alone) and whether it's unavailable.
struct Entry {
    class: &'static str,
    role: &'static str,
    label: &'static str,
    note: Option<&'static str>,
    checked: Option<bool>,
    unavailable: bool,
}

fn entry(item: MenuItem, archived: bool, turn_running: bool, compacting: bool, thinking_shown: bool) -> Entry {
    match item {
        MenuItem::Thinking => Entry {
            class: "conversation-menu-item conversation-menu-thinking",
            role: "menuitemcheckbox",
            label: if thinking_shown { "Hide thinking" } else { "Show thinking" },
            note: None,
            checked: Some(thinking_shown),
            unavailable: false,
        },
        // Unavailable while a turn runs (the server refuses then) and
        // while its own compaction runs; the reason is a line of its own,
        // since a phone shows no `title`.
        MenuItem::Compact => Entry {
            class: "conversation-menu-item conversation-menu-compact",
            role: "menuitem",
            label: if compacting { "Compacting…" } else { "Compact" },
            note: (turn_running && !compacting).then_some("once the turn ends"),
            checked: None,
            unavailable: turn_running || compacting,
        },
        MenuItem::Archive => Entry {
            class: "conversation-menu-item conversation-menu-archive",
            role: "menuitem",
            label: if archived { "Unarchive" } else { "Archive" },
            note: None,
            checked: None,
            unavailable: false,
        },
    }
}

/// Focuses a mounted element, if there is one.
fn focus(el: Option<MountedEvent>) {
    if let Some(el) = el {
        spawn(async move {
            let _ = el.set_focus(true).await;
        });
    }
}

/// The ☰ button and, while `open`, its menu. The header owns `open`, so
/// it can close the menu on a switch and a test can render it open.
#[component]
pub(super) fn ConversationMenu(
    open: Signal<bool>,
    items: Vec<MenuItem>,
    archived: bool,
    turn_running: bool,
    compacting: bool,
    on_action: EventHandler<MenuItem>,
) -> Element {
    let mut open = open;
    let mut button_el: Signal<Option<MountedEvent>> = use_signal(|| None);
    let mut item_els: Signal<Vec<Option<MountedEvent>>> = use_signal(Vec::new);
    // The item to focus once the menu renders open: the first after a
    // click, Enter, Space or ArrowDown, the last after ArrowUp.
    let mut focus_on_open: Signal<Option<usize>> = use_signal(|| None);
    let mut current = use_signal(|| 0usize);
    let count = items.len();
    let mut open_at = move |i: usize| {
        focus_on_open.set(Some(i));
        open.set(true);
    };
    let move_to = move |i: usize| focus(item_els.peek().get(i).cloned().flatten());
    let thinking_shown = super::transcript::SHOW_THINKING();
    rsx! {
        div { class: "conversation-menu-wrap",
            button {
                class: "conversation-menu-button",
                r#type: "button",
                aria_haspopup: "menu",
                aria_expanded: "{open()}",
                aria_controls: "conversation-menu",
                aria_label: "Conversation actions",
                title: "Conversation actions",
                onmounted: move |e| button_el.set(Some(e)),
                onclick: move |_| if open() { open.set(false) } else { open_at(0) },
                onkeydown: move |e: KeyboardEvent| match e.key() {
                    Key::ArrowDown => {
                        e.prevent_default();
                        open_at(0);
                    }
                    Key::ArrowUp => {
                        e.prevent_default();
                        open_at(count.saturating_sub(1));
                    }
                    _ => {}
                },
                "\u{2630}"
            }
            if open() {
                // A click outside lands here and only closes the menu: on
                // a phone the tap doesn't reach what's under it too.
                div { class: "conversation-menu-backdrop", onclick: move |_| open.set(false) }
                div {
                    id: "conversation-menu",
                    class: "conversation-menu",
                    role: "menu",
                    aria_label: "Conversation actions",
                    onkeydown: move |e: KeyboardEvent| {
                        let n = count.max(1);
                        let at = *current.peek();
                        let to = match e.key() {
                            Key::ArrowDown => Some((at + 1) % n),
                            Key::ArrowUp => Some((at + n - 1) % n),
                            Key::Home => Some(0),
                            Key::End => Some(n - 1),
                            Key::Escape => {
                                e.prevent_default();
                                e.stop_propagation();
                                open.set(false);
                                focus(button_el.peek().clone());
                                None
                            }
                            // Focus moves on as Tab would; the menu goes.
                            Key::Tab => {
                                open.set(false);
                                None
                            }
                            _ => None,
                        };
                        if let Some(i) = to {
                            e.prevent_default();
                            move_to(i);
                        }
                    },
                    for (i, item) in items.iter().copied().enumerate() {
                        {
                            let Entry { class, role, label, note, checked, unavailable } =
                                entry(item, archived, turn_running, compacting, thinking_shown);
                            rsx! {
                                button {
                                    key: "{i}",
                                    class,
                                    role,
                                    r#type: "button",
                                    tabindex: "-1",
                                    aria_checked: checked.map(|c| c.to_string()),
                                    aria_disabled: unavailable.then_some("true"),
                                    onmounted: move |e: MountedEvent| {
                                        let mut els = item_els.write();
                                        if els.len() <= i {
                                            els.resize(i + 1, None);
                                        }
                                        els[i] = Some(e.clone());
                                        if *focus_on_open.peek() == Some(i) {
                                            focus_on_open.set(None);
                                            focus(Some(e));
                                        }
                                    },
                                    onfocus: move |_| current.set(i),
                                    onclick: move |_| {
                                        if !unavailable {
                                            on_action.call(item);
                                        }
                                    },
                                    "{label}"
                                    if let Some(note) = note {
                                        span { class: "conversation-menu-note", "{note}" }
                                    }
                                }
                            }
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

    /// S1, S2 and S3 of the plan's header shapes.
    #[test]
    fn test_each_header_shape_offers_its_items() {
        use MenuItem::*;
        assert_eq!(menu_items(None, false), vec![Thinking, Compact], "S1: the tree hasn't the row yet");
        assert_eq!(menu_items(Some(false), true), vec![Thinking, Compact, Archive], "S2: a top-level conversation");
        assert_eq!(menu_items(None, true), vec![Thinking, Compact], "S3: an agent has no Archive");
    }

    #[derive(Clone, PartialEq)]
    struct Shown {
        items: Vec<MenuItem>,
        archived: bool,
        turn_running: bool,
        compacting: bool,
    }

    fn rendered(shown: Shown) -> String {
        fn app(shown: Shown) -> Element {
            let open = use_signal(|| true);
            rsx! {
                ConversationMenu {
                    open,
                    items: shown.items,
                    archived: shown.archived,
                    turn_running: shown.turn_running,
                    compacting: shown.compacting,
                    on_action: |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new_with_props(app, shown);
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    fn all(archived: bool, turn_running: bool, compacting: bool) -> Shown {
        Shown { items: menu_items(Some(archived), true), archived, turn_running, compacting }
    }

    /// The button says what it opens, and the open menu lists its items in
    /// order, Show thinking as a checkbox item.
    #[cfg(feature = "server")]
    #[test]
    fn test_the_open_menu_lists_its_items_in_order() {
        let html = rendered(all(false, false, false));
        assert!(html.contains(r#"aria-haspopup="menu""#) && html.contains(r#"aria-expanded="true""#), "{html}");
        assert!(html.contains(r#"aria-label="Conversation actions""#) && html.contains('\u{2630}'), "{html}");
        assert!(html.contains(r#"role="menu""#), "{html}");
        let at = |label: &str| html.find(label).unwrap_or_else(|| panic!("{label} in {html}"));
        assert!(at(">Show thinking<") < at(">Compact<") && at(">Compact<") < at(">Archive<"), "{html}");
        assert!(html.contains(r#"role="menuitemcheckbox""#) && html.contains(r#"aria-checked="false""#), "{html}");
        let archived = rendered(all(true, false, false));
        assert!(archived.contains(">Unarchive<"), "{archived}");
    }

    /// T18, moved from the popover's trigger: Compact waits for the turn
    /// to end, saying so on the item itself (a phone shows no `title`),
    /// and reads Compacting… while its own compaction runs.
    #[cfg(feature = "server")]
    #[test]
    fn test_the_compact_item_waits_for_the_turn_to_end() {
        let idle = rendered(all(false, false, false));
        assert!(!idle.contains(r#"aria-disabled="true""#), "{idle}");
        let running = rendered(all(false, true, false));
        assert!(running.contains(r#"aria-disabled="true""#), "{running}");
        assert!(running.contains("once the turn ends"), "{running}");
        let compacting = rendered(all(false, false, true));
        assert!(compacting.contains(r#"aria-disabled="true""#) && compacting.contains("Compacting…"), "{compacting}");
    }

    /// A closed menu is the button alone.
    #[cfg(feature = "server")]
    #[test]
    fn test_a_closed_menu_is_its_button_alone() {
        fn app() -> Element {
            let open = use_signal(|| false);
            rsx! { ConversationMenu { open, items: menu_items(Some(false), true), archived: false, turn_running: false, compacting: false, on_action: |_| {} } }
        }
        let mut dom = VirtualDom::new(app);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);
        assert!(html.contains("conversation-menu-button") && html.contains(r#"aria-expanded="false""#), "{html}");
        assert!(!html.contains(r#"role="menu""#), "{html}");
    }
}
