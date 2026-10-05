//! The message box and a new conversation's example asks.

use super::*;

/// Asks offered in an empty conversation (SME-41 D12): one each for code,
/// a repository and the web, the three things smelt is for.
pub(super) const EXAMPLE_ASKS: [&str; 3] = [
    "Write a Python script that prints the first 20 primes, then run it",
    "Clone https://github.com/pallets/itsdangerous and run its tests",
    "Find the latest stable Rust release and summarize what's new",
];

/// The message box with Send, and Stop while the model works. Sending
/// and stopping are the panel's (`on_send`, `on_stop`); a waiting
/// `pending_question` changes the box's placeholder.
#[component]
pub(super) fn Composer(
    state: Store<ConversationState>,
    mut input: Signal<String>,
    model_ready: Signal<bool>,
    on_send: EventHandler<()>,
    on_stop: EventHandler<MouseEvent>,
) -> Element {
    let turn_running = state.turn_running();
    let pending_question = state.pending_question();
    rsx! {
        form {
            class: "composer",
            onsubmit: move |event| {
                event.prevent_default();
                on_send.call(());
            },
            // The placeholder goes as soon as anything is typed; the label
            // stays, for a screen reader (SME-58).
            label { r#for: "message-box", class: "visually-hidden", "Message" }
            input {
                id: "message-box",
                r#type: "text",
                value: "{input}",
                // The message box waits while the model works in this
                // conversation, whoever started the turn; Stop is offered
                // instead.
                disabled: turn_running(),
                placeholder: if pending_question().is_some() { "Answer the question above, or write a reply instead" } else { "Type a message..." },
                oninput: move |e| input.set(e.value()),
            }
            button {
                r#type: "submit",
                disabled: turn_running() || !model_ready(),
                title: if model_ready() { "" } else { "Choose a model first" },
                "Send"
            }
            if turn_running() {
                button {
                    r#type: "button",
                    class: "stop-turn",
                    title: "Stop the model's current turn. Its sandbox, terminals and running commands keep going.",
                    onclick: move |evt| on_stop.call(evt),
                    "Stop"
                }
            }
        }
    }
}
