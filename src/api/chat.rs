use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::models::{Conversation, Message};

#[cfg(feature = "server")]
use crate::{anthropic, db};

/// Events relayed to the browser over the `send_message` server function's
/// `ServerEvents` stream — distinct from Anthropic's own SSE event shapes,
/// which `anthropic::stream` already reduces down to plain text deltas.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    Delta { text: String },
    Done { message_id: i64, content: String },
    Error { message: String },
}

#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations().await.map_err(ServerFnError::new)
}

#[post("/api/conversations")]
pub async fn create_conversation() -> ServerFnResult<Conversation> {
    db::create_conversation().await.map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    db::list_messages(id).await.map_err(ServerFnError::new)
}

#[cfg(feature = "server")]
fn anthropic_model() -> String {
    std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-opus-4-8".to_string())
}

#[post("/api/conversations/{id}/messages")]
pub async fn send_message(id: i64, content: String) -> ServerFnResult<ServerEvents<ChatEvent>> {
    db::create_message(id, "user", &content)
        .await
        .map_err(ServerFnError::new)?;

    let history = db::list_messages(id).await.map_err(ServerFnError::new)?;
    let request = anthropic::CreateMessageRequest {
        model: anthropic_model(),
        max_tokens: 4096,
        system: None,
        messages: history
            .into_iter()
            .map(|m| anthropic::AnthropicMessage {
                role: m.role,
                content: vec![anthropic::ContentBlock::Text { text: m.content }],
            })
            .collect(),
        stream: true,
    };

    Ok(ServerEvents::new(move |mut tx| async move {
        let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") else {
            let _ = tx
                .send(ChatEvent::Error {
                    message: "ANTHROPIC_API_KEY is not set on the server".to_string(),
                })
                .await;
            return;
        };

        let result = anthropic::stream::stream_anthropic_message(&api_key, &request, |delta| {
            // `on_delta` is a plain sync `FnMut`, but `SseTx::send` is only
            // `async` for API symmetry — it wraps a synchronous
            // `unbounded_send` with no `.await` inside, so send through the
            // underlying channel directly here. This keeps delta ordering
            // exact (a spawned task per delta could interleave out of order).
            if let Ok(event) = axum::response::sse::Event::default().json_data(ChatEvent::Delta {
                text: delta.to_string(),
            }) {
                let _ = tx.unbounded_send(event);
            }
        })
        .await;

        match result {
            Ok(assembled) => {
                match db::create_message(id, "assistant", &assembled).await {
                    Ok(saved) => {
                        let _ = tx
                            .send(ChatEvent::Done {
                                message_id: saved.id,
                                content: saved.content,
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(ChatEvent::Error {
                                message: format!("failed to save assistant reply: {e}"),
                            })
                            .await;
                    }
                }
            }
            Err(message) => {
                let _ = tx.send(ChatEvent::Error { message }).await;
            }
        }
    }))
}
