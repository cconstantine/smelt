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
/// closes (the underlying `FrameSubscription` this wraps then drops,
/// which stops the real screencast if this was the last viewer — see
/// `browsing::FrameSubscription`'s own doc comment).
#[get("/api/conversations/{id}/browsing/frames")]
pub async fn subscribe_browser_frames(
    id: i64,
) -> ServerFnResult<ServerEvents<crate::browsing::BrowserFrame>> {
    let mut subscription = crate::browsing::subscribe_frames(id).map_err(ServerFnError::new)?;
    Ok(ServerEvents::new(move |mut tx| async move {
        loop {
            match subscription.receiver.recv().await {
                Ok(frame) => {
                    let _ = tx.send(frame).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Frames are "latest wins" ephemeral data (see
                    // `browsing::FRAME_CHANNEL_CAPACITY`'s own doc
                    // comment) — a lagging viewer just misses some old
                    // ones, not something to reconcile like a message.
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
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
