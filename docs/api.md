# API — Dioxus Server Functions

There is no hand-rolled Axum API layer and no hand-written browser fetch client. Every endpoint is a plain async function in `src/api/chat.rs` decorated with `#[get(...)]` or `#[post(...)]` (from `dioxus::prelude`, available under the `fullstack` feature). The macro generates two different bodies for the same function signature:

- **Server build** (`feature = "server"`): registers a real Axum route (via `inventory::submit!`) and runs your actual function body.
- **Web build** (`feature = "web"`): replaces the body with a transparent HTTP call to that route, JSON-encoding the arguments and decoding the response.

So a frontend component calls `get_conversations()` exactly like calling a local async function — there is no separate `frontend/api.rs` fetch-helper file to keep in sync with the server routes.

```rust
#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations().await.map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    db::list_messages(id).await.map_err(ServerFnError::new)
}
```

Path parameters (`{id}`) map directly onto a same-named function argument. `ServerFnResult<T>` is `Result<T, ServerFnError>` — return it from every server function; the macro asserts on this at compile time.

## Streaming: `send_message`

`send_message` is the one endpoint that isn't plain request/response. It returns `ServerFnResult<ServerEvents<ChatEvent>>` — Dioxus fullstack's native SSE payload type:

```rust
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    Delta { text: String },
    Done { message_id: i64, content: String },
    Error { message: String },
}

#[post("/api/conversations/{id}/messages")]
pub async fn send_message(id: i64, content: String) -> ServerFnResult<ServerEvents<ChatEvent>> {
    db::create_message(id, "user", &content).await.map_err(ServerFnError::new)?;
    let request = /* build CreateMessageRequest from history */;

    Ok(ServerEvents::new(move |mut tx| async move {
        // stream_anthropic_message calls tx through a synchronous unbounded
        // send (not the async SseTx::send wrapper) so delta ordering is
        // exact — see the comment in send_message's body.
        let result = anthropic::stream::stream_anthropic_message(&api_key, &request, |delta| {
            let event = axum::response::sse::Event::default()
                .json_data(ChatEvent::Delta { text: delta.to_string() })
                .unwrap();
            let _ = tx.unbounded_send(event);
        }).await;
        // ... persist + send ChatEvent::Done or ChatEvent::Error
    }))
}
```

On the client, calling `send_message(id, content).await` returns the `ServerEvents<ChatEvent>` immediately (as soon as the connection opens), and the caller iterates it as events arrive:

```rust
let mut events = send_message(id, content).await?;
while let Some(event) = events.recv().await {
    match event? {
        ChatEvent::Delta { text } => streaming_text.write().push_str(&text),
        ChatEvent::Done { message_id, content } => { /* replace streaming state with the persisted message */ }
        ChatEvent::Error { message } => { /* show it */ }
    }
}
```

Because sending a message and opening its event stream are the same call, there's no POST-then-GET race to worry about — a hazard a two-endpoint hand-rolled SSE design would need to guard against explicitly.

## Current endpoints

| Function | Method + path | Notes |
|---|---|---|
| `get_conversations` | `GET /api/conversations` | ordered by `updated_at DESC` |
| `create_conversation` | `POST /api/conversations` | default title |
| `get_messages` | `GET /api/conversations/{id}/messages` | ordered by `created_at ASC` |
| `send_message` | `POST /api/conversations/{id}/messages` | streams the assistant reply, see above |

Not yet implemented (straightforward mechanical additions when needed): delete/rename a conversation, concurrent-send guarding.
