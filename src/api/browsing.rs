//! Browser-facing server functions for the live browsing panel —
//! separate from `anthropic::tools`' model-facing `open_browser_session`/
//! `browser_navigate`/... dispatch, same split `api::chat`'s
//! `get_sandbox_state`/`subscribe_conversation_events` already has
//! relative to the sandbox terminal tools. See
//! docs/projects/plans/web-browsing.md.

use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// One-shot check for the panel's initial load — is a session even open
/// right now? The panel shows an idle state if not, rather than trying to
/// subscribe to a frame stream that doesn't exist.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BrowsingState {
    pub session_open: bool,
}

#[get("/api/conversations/{id}/browsing")]
pub async fn get_browsing_state(id: i64) -> ServerFnResult<BrowsingState> {
    Ok(BrowsingState {
        session_open: crate::browsing::is_session_open(id),
    })
}

/// Live frame stream for the panel — opens once per conversation the
/// panel is visible for, closes when the browser tab navigates away or
/// closes. Built with `ServerEvents::from_stream`, not `ServerEvents::new`:
/// the response body *pulls* each frame only when the connection can take
/// it, so a slow viewer lags behind the small broadcast buffer and skips
/// stale frames instead of queueing them in memory, and a closed connection
/// drops the stream — and with it the viewer — straight away. (`new` runs
/// its closure as a detached task feeding an unbounded queue, which does
/// neither.)
#[get("/api/conversations/{id}/browsing/frames")]
pub async fn subscribe_browser_frames(
    id: i64,
) -> ServerFnResult<ServerEvents<crate::browsing::BrowserFrame>> {
    let subscription = crate::browsing::subscribe_frames(id).map_err(ServerFnError::new)?;
    Ok(ServerEvents::from_stream(crate::browsing::frame_stream(
        subscription,
    )))
}

/// Forwards one live-panel input event (mouse/keyboard) to the session's
/// real page.
#[post("/api/conversations/{id}/browsing/input")]
pub async fn send_browser_input(
    id: i64,
    event: crate::browsing::BrowserInputEvent,
) -> ServerFnResult<()> {
    crate::browsing::send_input(id, event)
        .await
        .map_err(ServerFnError::new)
}
