//! Interactive web browsing: a persistent browsing *session* per
//! conversation — open a page, read it, click/fill/navigate across
//! several tool calls, instead of `webfetch`'s fresh-page-per-call shape.
//! Plus a live panel: the user can watch and interact with the same real
//! page the model is browsing. See
//! SME-22.
//!
//! `BrowserFrame`/`BrowserInputEvent` are ungated — they cross the
//! client/server boundary as server-function payloads (`api::browsing`'s
//! frame stream and input endpoint), so the `web` build needs them too.
//! Everything else (the actual `chromiumoxide`-driven session logic) lives
//! in the `server`-only nested module, re-exported — same shape
//! `anthropic::tools` already uses for the same reason.

use serde::{Deserialize, Serialize};

/// One live-panel frame — a base64-encoded JPEG straight off the wire
/// (CDP's own `screencastFrame.data` is already base64; `chromiumoxide`'s
/// `Binary` type is a thin wrapper around that same string, not
/// separately decoded — confirmed against the vendored
/// `chromiumoxide_types` source — so no decode/re-encode round trip is
/// needed, just embed it directly as a `data:image/jpeg;base64,...` URI
/// on the frontend).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserFrame {
    pub data: String,
}

/// A live-panel input event, forwarded from the user's own browser to the
/// real session page. Mouse events carry coordinates in the *frame's own*
/// pixel space (the frontend translates an on-screen click position back
/// to frame pixels before sending) and dispatch as real
/// `Input.dispatchMouseEvent` commands. `TypeText` uses `Input.insertText`
/// (simple, but bypasses `keydown`/`keyup` listeners — acceptable for
/// ordinary typing) rather than simulating a `dispatchKeyEvent` per
/// character, which would need a full keycode table for arbitrary
/// characters; `PressKey` is for the small set of *named* keys
/// (Enter, Backspace, ...) a page's `keydown` handler might actually care
/// about, which does need a real `dispatchKeyEvent` — see
/// `server::named_key_event_fields`. `MouseMove` carries whether the
/// viewer's left button is actually held (read off the real DOM event, not
/// tracked server-side, so a button released outside the panel can't leave
/// the page stuck mid-drag) — a plain hover must reach the page as a move
/// with no buttons down, not a drag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum BrowserInputEvent {
    MouseMove { x: f64, y: f64, left_held: bool },
    MouseDown { x: f64, y: f64 },
    MouseUp { x: f64, y: f64 },
    Wheel { x: f64, y: f64, delta_x: f64, delta_y: f64 },
    TypeText { text: String },
    /// `modifiers` is CDP's bitmask (Alt=1, Ctrl=2, Meta=4, Shift=8), so
    /// Shift+Tab, Shift+Arrow and Ctrl+Backspace do what they would on a
    /// real keyboard.
    PressKey { key: String, modifiers: i64 },
}

#[cfg(feature = "server")]
mod server;

#[cfg(feature = "server")]
pub use server::{
    click, close_session, current_url, fill, frame_stream, go_back, navigate,
    normalize_address, open_session, read, send_input, subscribe_frames,
};

#[cfg(all(test, feature = "browser-test"))]
pub(crate) use server::browser_tests;
