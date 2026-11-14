//! The conversation header's ☰ menu (SME-513): the conversation's
//! actions behind one button rather than a row of them. After the APG
//! menu button pattern: the button (`aria-haspopup`, `aria-expanded`)
//! opens a `role=menu` with focus on an item; the arrows, Home and End
//! move inside it; Escape closes it back onto the button, Tab closes it
//! and moves on, and a click outside (its backdrop) closes it.

use super::*;
use crate::api::agents::ConversationNode;

/// One of the menu's actions, in the order the menu lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuItem {
    Thinking,
    Compact,
    Archive,
    Delete,
}

/// The items a header shows (SME-354 B18's shapes): Show thinking and
/// Compact always, even before the tree has the row (S1); Archive for a
/// listed top-level conversation (S2), never an agent (S3); Delete, last,
/// for any listed one, since its confirm counts the subtree.
pub(super) fn menu_items(archive: Option<bool>, listed: bool) -> Vec<MenuItem> {
    let mut items = vec![MenuItem::Thinking, MenuItem::Compact];
    if listed && archive.is_some() {
        items.push(MenuItem::Archive);
    }
    if listed {
        items.push(MenuItem::Delete);
    }
    items
}

/// The menu's own state: whether it's open, and whether its Delete is
/// armed (a second click confirms).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MenuState {
    pub open: bool,
    pub armed: bool,
}

/// What can happen to the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuEvent {
    /// The ☰ button, by a click or a key.
    Open,
    /// Delete's first click.
    Arm,
    /// Delete's second click.
    Confirm,
    Escape,
    /// A click on the backdrop.
    Outside,
    Tab,
    /// Any other item's action.
    OtherItem,
    /// Another conversation opened.
    Switch,
}

/// The plan's state table. Arming leaves the menu open (Q2); everything
/// else closes and disarms it. A list refetch, which comes on every turn
/// anywhere, is no event at all: the state lives in the header, not the
/// list, so it can't touch it (an armed Delete vanishing under one was
/// SME-505's signature).
pub(super) fn menu_after(event: MenuEvent) -> MenuState {
    match event {
        MenuEvent::Open => MenuState { open: true, armed: false },
        MenuEvent::Arm => MenuState { open: true, armed: true },
        MenuEvent::Confirm
        | MenuEvent::Escape
        | MenuEvent::Outside
        | MenuEvent::Tab
        | MenuEvent::OtherItem
        | MenuEvent::Switch => MenuState { open: false, armed: false },
    }
}

/// Applies `event` to the menu's two signals.
pub(super) fn apply(open: Signal<bool>, armed: Signal<bool>, event: MenuEvent) {
    let (mut open, mut armed) = (open, armed);
    let next = menu_after(event);
    if *open.peek() != next.open {
        open.set(next.open);
    }
    if *armed.peek() != next.armed {
        armed.set(next.armed);
    }
}

/// `id` and every conversation under it, at any depth.
pub(super) fn subtree_ids(conversations: &[ConversationNode], id: i64) -> Vec<i64> {
    let mut ids = vec![id];
    let mut frontier = vec![id];
    while let Some(parent) = frontier.pop() {
        for child in conversations.iter().filter(|c| c.parent_id == Some(parent)) {
            ids.push(child.id);
            frontier.push(child.id);
        }
    }
    ids
}

/// The armed Delete's label: how many agents go with the conversation, its
/// whole subtree, counted from the list the sidebar already has (SME-236).
pub(super) fn delete_confirm_label(conversations: &[ConversationNode], id: i64) -> String {
    match subtree_ids(conversations, id).len() - 1 {
        0 => "Confirm?".to_string(),
        1 => "Delete + 1 agent?".to_string(),
        n => format!("Delete + {n} agents?"),
    }
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
    /// Delete's two labels, idle and armed, in one spot (`TwoStepLabel`).
    two_step: bool,
}

/// What the items read from: the conversation's and the menu's state.
#[derive(Clone, Copy)]
struct Reads {
    archived: bool,
    turn_running: bool,
    compacting: bool,
    thinking_shown: bool,
    armed: bool,
    deleting: bool,
}

fn entry(item: MenuItem, r: Reads) -> Entry {
    let Reads { archived, turn_running, compacting, thinking_shown, armed, deleting } = r;
    let plain = |class, label| Entry { class, role: "menuitem", label, note: None, checked: None, unavailable: false, two_step: false };
    match item {
        MenuItem::Thinking => Entry {
            class: "conversation-menu-item conversation-menu-thinking",
            role: "menuitemcheckbox",
            label: if thinking_shown { "Hide thinking" } else { "Show thinking" },
            note: None,
            checked: Some(thinking_shown),
            ..plain("conversation-menu-item conversation-menu-thinking", "")
        },
        // Unavailable while a turn runs (the server refuses then) and
        // while its own compaction runs; the reason is a line of its own,
        // since a phone shows no `title`.
        MenuItem::Compact => Entry {
            note: (turn_running && !compacting).then_some("once the turn ends"),
            unavailable: turn_running || compacting,
            ..plain("conversation-menu-item conversation-menu-compact", if compacting { "Compacting…" } else { "Compact" })
        },
        MenuItem::Archive => plain("conversation-menu-item conversation-menu-archive", if archived { "Unarchive" } else { "Archive" }),
        // Destructive, last; armed it's red and reads its count; while its
        // DELETE is in flight it's unavailable, so no second one goes.
        MenuItem::Delete if deleting => Entry {
            unavailable: true,
            ..plain("conversation-menu-item conversation-menu-delete", "Deleting…")
        },
        MenuItem::Delete => Entry {
            two_step: true,
            ..plain(
                if armed {
                    "conversation-menu-item conversation-menu-delete confirm"
                } else {
                    "conversation-menu-item conversation-menu-delete"
                },
                "Delete",
            )
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

/// The ☰ button and, while `open`, its menu. The header owns `open` and
/// `armed` (Delete's first click), so it can close the menu on a switch
/// and a test can render it open; every change goes through `menu_after`.
#[component]
pub(super) fn ConversationMenu(
    open: Signal<bool>,
    armed: Signal<bool>,
    items: Vec<MenuItem>,
    archived: bool,
    turn_running: bool,
    compacting: bool,
    /// Delete's armed label (`delete_confirm_label`).
    #[props(into)]
    delete_confirm: String,
    /// Whether this conversation's DELETE is in flight.
    deleting: bool,
    on_action: EventHandler<MenuItem>,
) -> Element {
    let mut button_el: Signal<Option<MountedEvent>> = use_signal(|| None);
    let mut item_els: Signal<Vec<Option<MountedEvent>>> = use_signal(Vec::new);
    // The item to focus once the menu renders open: the first after a
    // click, Enter, Space or ArrowDown, the last after ArrowUp.
    let mut focus_on_open: Signal<Option<usize>> = use_signal(|| None);
    let mut current = use_signal(|| 0usize);
    let count = items.len();
    let mut open_at = move |i: usize| {
        focus_on_open.set(Some(i));
        apply(open, armed, MenuEvent::Open);
    };
    let move_to = move |i: usize| focus(item_els.peek().get(i).cloned().flatten());
    let reads = Reads {
        archived,
        turn_running,
        compacting,
        thinking_shown: super::transcript::SHOW_THINKING(),
        armed: armed(),
        deleting,
    };
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
                onclick: move |_| if open() { apply(open, armed, MenuEvent::Outside) } else { open_at(0) },
                // Already open (a click opened it and focus came back to
                // ☰), the arrows move into the menu and Escape closes it.
                onkeydown: move |e: KeyboardEvent| match (e.key(), open()) {
                    (Key::ArrowDown, was_open) => {
                        e.prevent_default();
                        if was_open { move_to(0) } else { open_at(0) }
                    }
                    (Key::ArrowUp, was_open) => {
                        e.prevent_default();
                        let last = count.saturating_sub(1);
                        if was_open { move_to(last) } else { open_at(last) }
                    }
                    (Key::Escape, true) => {
                        e.prevent_default();
                        e.stop_propagation();
                        apply(open, armed, MenuEvent::Escape);
                    }
                    _ => {}
                },
                "\u{2630}"
            }
            if open() {
                // A click outside lands here and only closes the menu: on
                // a phone the tap doesn't reach what's under it too.
                div { class: "conversation-menu-backdrop", onclick: move |_| apply(open, armed, MenuEvent::Outside) }
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
                                apply(open, armed, MenuEvent::Escape);
                                focus(button_el.peek().clone());
                                None
                            }
                            // Focus moves on as Tab would; the menu goes.
                            Key::Tab => {
                                apply(open, armed, MenuEvent::Tab);
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
                            let Entry { class, role, label, note, checked, unavailable, two_step } = entry(item, reads);
                            let confirm = delete_confirm.clone();
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
                                        if unavailable {
                                            return;
                                        }
                                        // Delete's first click arms it in place, the
                                        // menu open and focus kept (Q2); its second
                                        // confirms. Any other item closes the menu.
                                        // Focus goes back to ☰ as the menu closes,
                                        // not to the page's body; Compact's popover
                                        // takes it instead (review 1, A-L4).
                                        // Read first: a guard in the match's
                                        // scrutinee lives through its arms, and
                                        // `apply` writes the same signal.
                                        let was_armed = *armed.peek();
                                        match (item, was_armed) {
                                            (MenuItem::Delete, false) => apply(open, armed, MenuEvent::Arm),
                                            (MenuItem::Delete, true) => {
                                                apply(open, armed, MenuEvent::Confirm);
                                                focus(button_el.peek().clone());
                                                on_action.call(item);
                                            }
                                            _ => {
                                                apply(open, armed, MenuEvent::OtherItem);
                                                if item != MenuItem::Compact {
                                                    focus(button_el.peek().clone());
                                                }
                                                on_action.call(item);
                                            }
                                        }
                                    },
                                    if two_step {
                                        crate::frontend::pages::TwoStepLabel { armed: reads.armed, idle: label, confirm }
                                    } else {
                                        "{label}"
                                    }
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

    /// S1, S2 and S3 of the plan's header shapes. Delete waits for the
    /// tree: its confirm counts the subtree.
    #[test]
    fn test_each_header_shape_offers_its_items() {
        use MenuItem::*;
        assert_eq!(menu_items(None, false), vec![Thinking, Compact], "S1: the tree hasn't the row yet");
        assert_eq!(menu_items(Some(false), true), vec![Thinking, Compact, Archive, Delete], "S2: a top-level conversation");
        assert_eq!(menu_items(None, true), vec![Thinking, Compact, Delete], "S3: an agent has no Archive");
    }

    /// The plan's state table: what each event leaves of `{open, armed}`.
    /// Arming leaves the menu open (Q2). A list refetch isn't an event at
    /// all (SME-505); the conversation_menu browser scenario checks that.
    #[test]
    fn test_each_event_moves_the_menu_as_the_state_table_says() {
        use MenuEvent::*;
        let closed = MenuState { open: false, armed: false };
        assert_eq!(menu_after(Open), MenuState { open: true, armed: false });
        assert_eq!(menu_after(Arm), MenuState { open: true, armed: true }, "arming keeps the menu open");
        for event in [Confirm, Escape, Outside, Tab, OtherItem, Switch] {
            assert_eq!(menu_after(event), closed, "{event:?} closes and disarms");
        }
    }

    /// Moved from the sidebar (SME-236): the armed label counts the whole
    /// subtree.
    #[test]
    fn test_the_delete_label_counts_the_whole_subtree() {
        let list = vec![node(1, None), node(2, Some(1)), node(3, Some(1)), node(4, Some(2)), node(5, None)];
        assert_eq!(delete_confirm_label(&list, 1), "Delete + 3 agents?");
        assert_eq!(delete_confirm_label(&list, 2), "Delete + 1 agent?");
        assert_eq!(delete_confirm_label(&list, 5), "Confirm?");
        let mut gone = subtree_ids(&list, 1);
        gone.sort();
        assert_eq!(gone, vec![1, 2, 3, 4]);
    }

    fn node(id: i64, parent_id: Option<i64>) -> crate::api::agents::ConversationNode {
        let at = chrono::NaiveDateTime::default();
        crate::models::Conversation { id, title: format!("c{id}"), created_at: at, updated_at: at, parent_id, archived_at: None }
            .into()
    }

    /// Delete is last; armed, it carries `confirm` and its count, and
    /// stays a menu item; in flight, it reads Deleting… and is unavailable.
    #[cfg(feature = "server")]
    #[test]
    fn test_delete_is_last_and_arms_in_place() {
        let idle = rendered(all(false, false, false));
        let at = |label: &str| idle.find(label).unwrap_or_else(|| panic!("{label} in {idle}"));
        assert!(at(">Archive<") < at("conversation-menu-delete"), "{idle}");
        assert!(!idle.contains("conversation-menu-delete confirm"), "{idle}");
        let armed = rendered(Shown { armed: true, ..all(false, false, false) });
        assert!(armed.contains("conversation-menu-delete confirm"), "{armed}");
        assert!(armed.contains("Delete + 2 agents?"), "{armed}");
        assert!(armed.contains(r#"role="menu""#), "the menu stays open: {armed}");
        let deleting = rendered(Shown { deleting: true, ..all(false, false, false) });
        assert!(deleting.contains("Deleting…"), "{deleting}");
        assert_eq!(deleting.matches(r#"aria-disabled="true""#).count(), 1, "{deleting}");
    }

    #[derive(Clone, PartialEq)]
    struct Shown {
        items: Vec<MenuItem>,
        archived: bool,
        turn_running: bool,
        compacting: bool,
        armed: bool,
        deleting: bool,
        /// The global Show thinking, set before the first render.
        thinking: bool,
    }

    fn rendered(shown: Shown) -> String {
        fn app(shown: Shown) -> Element {
            use_hook(|| {
                if shown.thinking {
                    *super::super::transcript::SHOW_THINKING.write() = true;
                }
            });
            let open = use_signal(|| true);
            let armed = use_signal(|| shown.armed);
            rsx! {
                ConversationMenu {
                    open,
                    armed,
                    items: shown.items,
                    archived: shown.archived,
                    turn_running: shown.turn_running,
                    compacting: shown.compacting,
                    delete_confirm: "Delete + 2 agents?",
                    deleting: shown.deleting,
                    on_action: |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new_with_props(app, shown);
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    fn all(archived: bool, turn_running: bool, compacting: bool) -> Shown {
        Shown { items: menu_items(Some(archived), true), archived, turn_running, compacting, armed: false, deleting: false, thinking: false }
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

    /// Review 1, B-L4: with thinking shown, the checkbox item reads Hide
    /// thinking and is checked.
    #[cfg(feature = "server")]
    #[test]
    fn test_shown_thinking_reads_hide_thinking_checked() {
        let html = rendered(Shown { thinking: true, ..all(false, false, false) });
        assert!(html.contains(">Hide thinking<") && !html.contains(">Show thinking<"), "{html}");
        assert!(html.contains(r#"role="menuitemcheckbox""#) && html.contains(r#"aria-checked="true""#), "{html}");
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
            let armed = use_signal(|| false);
            rsx! {
                ConversationMenu {
                    open,
                    armed,
                    items: menu_items(Some(false), true),
                    archived: false,
                    turn_running: false,
                    compacting: false,
                    delete_confirm: "Confirm?",
                    deleting: false,
                    on_action: |_| {},
                }
            }
        }
        let mut dom = VirtualDom::new(app);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);
        assert!(html.contains("conversation-menu-button") && html.contains(r#"aria-expanded="false""#), "{html}");
        assert!(!html.contains(r#"role="menu""#), "{html}");
    }
}
