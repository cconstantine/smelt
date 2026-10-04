//! The streaming reply and the working line.

use super::*;
use crate::markdown::Markdown;

/// How long a turn has been running, as shown on its "Working…" line:
/// `8s`, `2m 05s`.
pub(super) fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// The reply as it streams. Its own component, reading only
/// `streaming_reply`, so a delta re-renders the bubble and not the
/// transcript around it (SME-57).
#[component]
pub(super) fn StreamingReply(state: Store<ConversationState>, on_media_load: EventHandler<()>) -> Element {
    let streaming_reply = state.streaming_reply();
    let reply = streaming_reply.read().as_ref().filter(|text| !text.is_empty()).cloned();
    rsx! {
        if let Some(reply) = reply {
            div { class: "message message-assistant message-streaming",
                Markdown { source: reply, on_media_load }
            }
        }
    }
}

/// The "Working…" line while a turn runs (SME-41 D1). Its own component,
/// reading only `turn_elapsed`, so the once-a-second tick re-renders this
/// line and not the transcript (SME-57).
#[component]
pub(super) fn WorkingLine(state: Store<ConversationState>) -> Element {
    let turn_elapsed = state.turn_elapsed();
    rsx! {
        div { class: "turn-working", role: "status",
            span { class: "turn-working-dot" }
            span { "Working… {format_elapsed(turn_elapsed())}" }
        }
    }
}
