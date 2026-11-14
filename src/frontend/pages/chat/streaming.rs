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

/// The working line while a call waits at the five-hour usage gate
/// (SME-523): when it resumes in the viewer's local time
/// (`tz_offset_minutes` ahead of UTC), and how long that is from `now`.
pub(super) fn held_text(until: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>, tz_offset_minutes: i32) -> String {
    let lead = "Waiting for the Claude 5-hour usage window";
    let left = (until - now).num_seconds();
    if left <= 0 {
        return format!("{lead} · resumes any moment");
    }
    let local = until + chrono::Duration::minutes(i64::from(tz_offset_minutes));
    // Part of a minute counts as one, so it never says "in 0 min".
    let minutes = (left + 59) / 60;
    let span = match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    };
    format!("{lead} · resumes about {} (in {span})", local.format("%H:%M"))
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

/// The "Working…" line while a turn runs (SME-41 D1), or, while its call
/// waits at the five-hour usage gate, when it resumes (SME-523). Its own
/// component, reading only `turn_elapsed` and `usage_hold`, so the
/// once-a-second tick re-renders this line and not the transcript
/// (SME-57); the tick also counts the hold's minutes down.
#[component]
pub(super) fn WorkingLine(state: Store<ConversationState>, tz_offset_minutes: Signal<i32>) -> Element {
    let turn_elapsed = state.turn_elapsed();
    let usage_hold = state.usage_hold();
    let elapsed = turn_elapsed();
    let text = match usage_hold() {
        Some(until) => held_text(until, chrono::Utc::now(), tz_offset_minutes()),
        None => format!("Working… {}", format_elapsed(elapsed)),
    };
    rsx! {
        div { class: "turn-working", role: "status",
            span { class: "turn-working-dot" }
            span { "{text}" }
        }
    }
}
