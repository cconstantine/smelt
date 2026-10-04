//! The live browsing panel's input mapping.

use super::super::*;

/// Maps a live-panel keydown to a `BrowserInputEvent`, or `None` for a
/// key that isn't meaningful to forward (a bare modifier like Shift/Alt/
/// Control on its own, an unrecognized named key, ...). A printable
/// character forwards as `TypeText`; a named key forwards as `PressKey`
/// only if `browsing::server::named_key_event_fields` (checked
/// server-side too — this is just avoiding an obviously-doomed round
/// trip) recognizes it. Unconditional (not web-gated): called from the
/// main render body's `onkeydown` handler, part of the same shared rsx
/// tree the server target compiles too — same reason `todo_status_class`
/// is unconditional. A character typed with Ctrl/Cmd held is a shortcut,
/// not text (Ctrl+V must not type a "v"), so it isn't forwarded — the
/// caller then leaves it to the viewer's own browser. Ctrl+Alt is let
/// through: that's how AltGr reports itself on Windows, and AltGr is
/// ordinary typing on many layouts. A named key carries its modifiers, so
/// Shift+Tab or Ctrl+Backspace do what they would on a real keyboard.
pub(in super::super) fn browser_input_event_for_key(
    key: keyboard_types::Key,
    modifiers: keyboard_types::Modifiers,
) -> Option<BrowserInputEvent> {
    match key {
        keyboard_types::Key::Character(_)
            if (modifiers.ctrl() || modifiers.meta()) && !modifiers.alt() =>
        {
            None
        }
        keyboard_types::Key::Character(text) => Some(BrowserInputEvent::TypeText { text }),
        named => {
            let name = named.to_string();
            matches!(
                name.as_str(),
                "Enter"
                    | "Backspace"
                    | "Tab"
                    | "Escape"
                    | "Delete"
                    | "ArrowUp"
                    | "ArrowDown"
                    | "ArrowLeft"
                    | "ArrowRight"
            )
            .then(|| BrowserInputEvent::PressKey {
                key: name,
                modifiers: cdp_modifiers(modifiers),
            })
        }
    }
}

/// CDP's modifier bitmask for `Input.dispatchKeyEvent`.
pub(in super::super) fn cdp_modifiers(modifiers: keyboard_types::Modifiers) -> i64 {
    [
        (modifiers.alt(), 1),
        (modifiers.ctrl(), 2),
        (modifiers.meta(), 4),
        (modifiers.shift(), 8),
    ]
    .into_iter()
    .filter(|(held, _)| *held)
    .map(|(_, bit)| bit)
    .sum()
}

/// The live panel address bar's text: the page's URL, except while the
/// viewer is typing, when an arriving URL change mustn't overwrite them.
pub(in super::super) fn address_bar_value(editing: bool, draft: &str, url: Option<&str>) -> String {
    if editing {
        draft.to_string()
    } else {
        url.unwrap_or_default().to_string()
    }
}

/// Chromium scrolls 40px per wheel "line".
pub(in super::super) const WHEEL_PIXELS_PER_LINE: f64 = 40.0;

/// A wheel "page" is one live-panel frame (`browsing::server`'s pinned
/// 1280x800 viewport).
pub(in super::super) const WHEEL_PIXELS_PER_PAGE: (f64, f64) = (1280.0, 800.0);

/// The frame's width in the remote page's pixels (`browsing::server`'s
/// pinned 1280x800 viewport).
pub(in super::super) const FRAME_WIDTH: f64 = 1280.0;

/// A point on the live-panel frame as shown (`shown_width` pixels wide,
/// scaled to fit the panel) in the remote page's own pixels.
pub(in super::super) fn frame_point(x: f64, y: f64, shown_width: f64) -> (f64, f64) {
    if shown_width <= 0.0 {
        return (x, y);
    }
    let scale = FRAME_WIDTH / shown_width;
    (x * scale, y * scale)
}

/// A wheel event's delta in pixels, whatever unit the viewer's browser
/// reported it in — Firefox reports lines, so passing the raw number
/// through would scroll ~3px per notch.
pub(in super::super) fn wheel_delta_pixels(delta: WheelDelta) -> (f64, f64) {
    match delta {
        WheelDelta::Pixels(v) => (v.x, v.y),
        WheelDelta::Lines(v) => (v.x * WHEEL_PIXELS_PER_LINE, v.y * WHEEL_PIXELS_PER_LINE),
        WheelDelta::Pages(v) => (v.x * WHEEL_PIXELS_PER_PAGE.0, v.y * WHEEL_PIXELS_PER_PAGE.1),
    }
}

/// Collapses each run of back-to-back mouse moves (for the same
/// conversation) down to its last one — only the pointer's latest position
/// matters, and forwarding every intermediate one would let a fast-moving
/// mouse queue up far more requests than the page needs. Anything else
/// keeps its place and order.
pub(in super::super) fn coalesce_mouse_moves(batch: Vec<(i64, BrowserInputEvent)>) -> Vec<(i64, BrowserInputEvent)> {
    let mut out: Vec<(i64, BrowserInputEvent)> = Vec::with_capacity(batch.len());
    for item in batch {
        let replaces_last = matches!(
            (out.last(), &item),
            (
                Some((last_id, BrowserInputEvent::MouseMove { .. })),
                (id, BrowserInputEvent::MouseMove { .. }),
            ) if last_id == id
        );
        if replaces_last {
            out.pop();
        }
        out.push(item);
    }
    out
}

/// The live browsing panel: the address bar and the session's frames,
/// with mouse, wheel and key input on a frame forwarded to the page. Going
/// to an address is the panel's (`on_navigate`).
#[component]
pub(in super::super) fn BrowsingPanel(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    mut frame_shown_width: Signal<f64>,
    browser_input: Coroutine<(i64, BrowserInputEvent)>,
    on_navigate: EventHandler<()>,
) -> Element {
    let browsing_url = state.browsing_url();
    let browsing_frame = state.browsing_frame();
    let mut address_draft = state.address_draft();
    let mut address_editing = state.address_editing();
    let address_pending = state.address_pending();
    let mut address_error = state.address_error();
    rsx! {
        aside { class: "browsing-panel",
            h3 { "Live Browser" }
            form {
                class: "browsing-address-bar",
                onsubmit: move |event| {
                    event.prevent_default();
                    on_navigate.call(());
                },
                input {
                    class: "browsing-address-input",
                    r#type: "text",
                    spellcheck: "false",
                    autocomplete: "off",
                    aria_label: "Address",
                    placeholder: "Enter an address",
                    disabled: address_pending(),
                    value: address_bar_value(
                        address_editing(),
                        &address_draft(),
                        browsing_url().as_deref(),
                    ),
                    onfocus: move |_| {
                        if !address_editing() {
                            address_draft.set(browsing_url().unwrap_or_default());
                            address_editing.set(true);
                        }
                    },
                    oninput: move |e| {
                        address_draft.set(e.value());
                        address_editing.set(true);
                    },
                    onblur: move |_| {
                        if !address_pending() {
                            address_editing.set(false);
                        }
                    },
                    onkeydown: move |e: Event<KeyboardData>| {
                        if e.data().key() == keyboard_types::Key::Escape {
                            address_editing.set(false);
                            address_error.set(None);
                        }
                    },
                }
            }
            if let Some(error) = address_error() {
                p { class: "browsing-address-error", role: "alert", "{error}" }
            }
            div {
                class: "browsing-panel-frame-wrap",
                tabindex: "0",
                oncontextmenu: move |evt| evt.prevent_default(),
                onresize: move |evt: Event<ResizeData>| {
                    if let Ok(size) = evt.data().get_content_box_size() {
                        frame_shown_width.set(size.width);
                    }
                },
                onmousemove: move |evt: Event<MouseData>| {
                    let Some(id) = selected() else { return };
                    let p = evt.data().element_coordinates();
                    let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                    let left_held = evt.data().held_buttons().contains(MouseButton::Primary);
                    browser_input.send((id, BrowserInputEvent::MouseMove { x, y, left_held }));
                },
                onmousedown: move |evt: Event<MouseData>| {
                    if evt.data().trigger_button() != Some(MouseButton::Primary) {
                        return;
                    }
                    let Some(id) = selected() else { return };
                    let p = evt.data().element_coordinates();
                    let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                    browser_input.send((id, BrowserInputEvent::MouseDown { x, y }));
                },
                onmouseup: move |evt: Event<MouseData>| {
                    if evt.data().trigger_button() != Some(MouseButton::Primary) {
                        return;
                    }
                    let Some(id) = selected() else { return };
                    let p = evt.data().element_coordinates();
                    let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                    browser_input.send((id, BrowserInputEvent::MouseUp { x, y }));
                },
                onwheel: move |evt: Event<WheelData>| {
                    let Some(id) = selected() else { return };
                    let p = evt.data().element_coordinates();
                    let (x, y) = frame_point(p.x, p.y, frame_shown_width());
                    let (delta_x, delta_y) = wheel_delta_pixels(evt.data().delta());
                    browser_input.send((
                        id,
                        BrowserInputEvent::Wheel {
                            x,
                            y,
                            delta_x,
                            delta_y,
                        },
                    ));
                },
                onkeydown: move |evt: Event<KeyboardData>| {
                    let Some(id) = selected() else { return };
                    let Some(input_event) = browser_input_event_for_key(
                        evt.data().key(),
                        evt.data().modifiers(),
                    ) else {
                        // Not ours to handle (a shortcut, a bare
                        // modifier) — leave it to the viewer's browser.
                        return;
                    };
                    evt.prevent_default();
                    browser_input.send((id, input_event));
                },
                if let Some(data) = browsing_frame() {
                    img {
                        class: "browsing-panel-frame",
                        src: "data:image/jpeg;base64,{data}",
                        alt: "Live browsing session",
                    }
                } else {
                    div { class: "browsing-panel-empty", "Waiting for the first frame…" }
                }
            }
        }
    }
}
