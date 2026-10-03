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
