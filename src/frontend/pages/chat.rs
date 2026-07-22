use dioxus::prelude::*;

use crate::api::chat::{ChatEvent, create_conversation, get_conversations, get_messages, send_message};
use crate::models::{Conversation, Message};

#[component]
pub fn Chat() -> Element {
    let selected: Signal<Option<i64>> = use_signal(|| None);

    rsx! {
        div { class: "chat-layout",
            ConversationSidebar { selected }
            ChatPanel { selected }
        }
    }
}

#[component]
fn ConversationSidebar(mut selected: Signal<Option<i64>>) -> Element {
    let initial_conversations = use_resource(get_conversations);
    let mut conversations: Signal<Vec<Conversation>> = use_signal(Vec::new);
    let mut loaded = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);

    use_effect(move || {
        if let Some(result) = initial_conversations() {
            match result {
                Ok(list) => conversations.set(list),
                Err(e) => error.set(Some(e.to_string())),
            }
            loaded.set(true);
        }
    });

    let new_conversation = move |_| {
        spawn(async move {
            match create_conversation().await {
                Ok(conversation) => {
                    let id = conversation.id;
                    conversations.write().insert(0, conversation);
                    selected.set(Some(id));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    rsx! {
        aside { class: "sidebar",
            button { class: "new-conversation", onclick: new_conversation, "New conversation" }
            if let Some(err) = error() {
                p { class: "error", "{err}" }
            }
            if !loaded() {
                p { class: "muted", "Loading..." }
            } else if conversations().is_empty() {
                p { class: "muted", "No conversations yet" }
            } else {
                div { class: "conversation-list",
                    for conversation in conversations() {
                        div {
                            key: "{conversation.id}",
                            class: if selected() == Some(conversation.id) { "conversation-item active" } else { "conversation-item" },
                            onclick: move |_| selected.set(Some(conversation.id)),
                            "{conversation.title}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ChatPanel(selected: Signal<Option<i64>>) -> Element {
    let initial_messages = use_resource(move || {
        let id = selected();
        async move {
            match id {
                Some(id) => Some(get_messages(id).await),
                None => None,
            }
        }
    });

    let mut messages: Signal<Vec<Message>> = use_signal(Vec::new);
    let mut load_error: Signal<Option<String>> = use_signal(|| None);
    let mut streaming_text: Signal<String> = use_signal(String::new);
    let mut is_streaming = use_signal(|| false);
    let mut stream_error: Signal<Option<String>> = use_signal(|| None);
    let mut input = use_signal(String::new);
    let mut next_temp_id = use_signal(|| -1i64);

    use_effect(move || {
        match initial_messages() {
            Some(Some(Ok(list))) => {
                messages.set(list);
                load_error.set(None);
            }
            Some(Some(Err(e))) => load_error.set(Some(e.to_string())),
            Some(None) => messages.set(Vec::new()),
            None => {}
        }
    });

    let mut send = move || {
        let Some(id) = selected() else { return };
        let content = input();
        if content.trim().is_empty() {
            return;
        }
        input.set(String::new());

        let temp_id = next_temp_id();
        next_temp_id.set(temp_id - 1);
        messages.write().push(Message {
            id: temp_id,
            conversation_id: id,
            role: "user".to_string(),
            content: content.clone(),
            created_at: chrono::Utc::now().naive_utc(),
        });

        spawn(async move {
            is_streaming.set(true);
            stream_error.set(None);
            streaming_text.set(String::new());

            match send_message(id, content).await {
                Ok(mut events) => {
                    while let Some(event) = events.recv().await {
                        match event {
                            Ok(ChatEvent::Delta { text }) => {
                                streaming_text.write().push_str(&text);
                            }
                            Ok(ChatEvent::Done { message_id, content }) => {
                                messages.write().push(Message {
                                    id: message_id,
                                    conversation_id: id,
                                    role: "assistant".to_string(),
                                    content,
                                    created_at: chrono::Utc::now().naive_utc(),
                                });
                                streaming_text.set(String::new());
                            }
                            Ok(ChatEvent::Error { message }) => {
                                stream_error.set(Some(message));
                            }
                            Err(e) => stream_error.set(Some(e.to_string())),
                        }
                    }
                }
                Err(e) => stream_error.set(Some(e.to_string())),
            }

            is_streaming.set(false);
        });
    };

    rsx! {
        section { class: "chat-panel",
            match selected() {
                None => rsx! {
                    div { class: "empty-state", "Select or start a conversation" }
                },
                Some(_) => rsx! {
                    div { class: "messages",
                        if let Some(err) = load_error() {
                            p { class: "error", "Error loading messages: {err}" }
                        }
                        for message in messages() {
                            div { key: "{message.id}", class: "message message-{message.role}", "{message.content}" }
                        }
                        if is_streaming() {
                            div { class: "message message-assistant message-streaming", "{streaming_text}" }
                        }
                        if let Some(err) = stream_error() {
                            p { class: "error", "{err}" }
                        }
                    }
                    form {
                        class: "composer",
                        onsubmit: move |event| {
                            event.prevent_default();
                            send();
                        },
                        input {
                            r#type: "text",
                            value: "{input}",
                            disabled: is_streaming(),
                            placeholder: "Type a message...",
                            oninput: move |e| input.set(e.value()),
                        }
                        button { r#type: "submit", disabled: is_streaming(), "Send" }
                    }
                },
            }
        }
    }
}
