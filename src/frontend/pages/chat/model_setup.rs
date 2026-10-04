//! What the transcript says when a turn or a background wake-up fails,
//! including the "no model provider" note with its link to set one up.

use super::*;

/// The open conversation's last turn error, and the last background
/// notification that couldn't reach the model.
#[component]
pub(super) fn ModelNotes(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
    stream_errors: Signal<HashMap<i64, String>>,
) -> Element {
    let notification_delivery_error = state.notification_delivery_error();
    let stream_error = move || selected().and_then(|id| stream_errors.read().get(&id).cloned());
    rsx! {
        if let Some(err) = stream_error() {
            if err == crate::providers::NO_MODEL_CONFIGURED {
                p { class: "model-picker-setup", role: "status",
                    "{err} "
                    Link { to: Route::ProvidersRoute {}, "Model providers" }
                }
            } else {
                p { class: "error", "{err}" }
            }
        }
        if let Some(err) = notification_delivery_error() {
            if err == crate::providers::NO_MODEL_CONFIGURED {
                p { class: "model-picker-setup", role: "status",
                    "A finished command or task is waiting for the model, but there's no model to send it to. "
                    Link { to: Route::ProvidersRoute {}, "Model providers" }
                }
            } else {
                p { class: "error", "A background notification failed to reach the model: {err}" }
            }
        }
    }
}
