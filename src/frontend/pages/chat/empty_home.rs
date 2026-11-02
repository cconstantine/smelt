//! What the chat page shows with no conversation open (SME-123 DR4): a
//! way on, not only "Select or start a conversation". On a phone the
//! sidebar's New conversation is behind its button, so this is the one in
//! reach; on a first run it says to add a model provider first.

use dioxus::prelude::*;

use super::server_error_message;
use crate::frontend::Route;
use crate::api::chat::create_conversation;
use crate::api::providers::list_providers;

#[component]
pub(super) fn EmptyHome() -> Element {
    let navigator = use_navigator();
    let providers = use_resource(|| async move { list_providers().await });
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let no_provider = matches!(&*providers.read(), Some(Ok(list)) if list.is_empty());
    let start = move |_| {
        spawn(async move {
            match create_conversation().await {
                Ok(conversation) => {
                    navigator.push(Route::ConversationRoute { id: conversation.id });
                }
                Err(e) => error.set(Some(server_error_message(&e))),
            }
        });
    };
    rsx! {
        div { class: "empty-state",
            if no_provider {
                p { "smelt needs a model to talk to first." }
                Link { to: Route::ProviderNewRoute {}, class: "empty-new-conversation empty-add-provider", "Add a model provider" }
            } else {
                p { "Pick a conversation from the list, or start one." }
                button { class: "empty-new-conversation", r#type: "button", onclick: start, "New conversation" }
            }
            super::ErrorText { message: error() }
        }
    }
}
