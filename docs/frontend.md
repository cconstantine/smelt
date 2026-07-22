# Frontend

Dioxus components under `src/frontend/`, rendered via **fullstack SSR**: the server renders real HTML for the initial page load (no blank-page-then-WASM-boot flash), then the WASM client hydrates it for interactivity.

## Structure

```
frontend/
  mod.rs           # App (router root), Route enum
  pages/
    mod.rs
    chat.rs        # Chat, ConversationSidebar, ChatPanel
```

`App` mounts a single stylesheet asset and the router:

```rust
#[component]
pub fn App() -> Element {
    rsx! {
        document::Stylesheet { href: asset!("/assets/chat.css") }
        Router::<Route> {}
    }
}
```

There's one route today (`Route::Chat {}` at `/`). A future per-conversation URL (`/c/{id}`) is a natural next step if deep-linking becomes useful, but v1 keeps conversation selection as in-component state (a `Signal<Option<i64>>`) rather than a route param.

## Calling server functions

No fetch layer to write — call the functions from `src/api/chat.rs` directly, same as any other async function. See [api.md](api.md) for how the isomorphic call works. A typical load-on-select pattern:

```rust
let initial_messages = use_resource(move || {
    let id = selected();               // read the Signal *before* the async block —
    async move {                       // this is what makes use_resource re-run
        match id {                     // when `selected` changes.
            Some(id) => Some(get_messages(id).await),
            None => None,
        }
    }
});

let mut messages: Signal<Vec<Message>> = use_signal(Vec::new);
use_effect(move || {
    if let Some(Some(Ok(list))) = initial_messages() {
        messages.set(list);
    }
});
```

The `use_resource` + `use_effect`-into-a-plain-signal pairing (rather than reading the resource directly in the render body) is used deliberately: it gives a stable, independently-updatable `Signal<Vec<Message>>` that the streaming send handler can also push into, without fighting the resource's own lifecycle.

## Streaming into the UI

`ChatPanel`'s send handler is the one place that consumes `ServerEvents` directly:

```rust
let mut events = send_message(id, content).await?;
while let Some(event) = events.recv().await {
    match event? {
        ChatEvent::Delta { text } => streaming_text.write().push_str(&text),
        ChatEvent::Done { message_id, content } => {
            messages.write().push(Message { id: message_id, content, .. });
            streaming_text.set(String::new());
        }
        ChatEvent::Error { message } => stream_error.set(Some(message)),
    }
}
```

`streaming_text` is rendered as a separate trailing bubble while `is_streaming` is true, so the growing reply is visible token-by-token; it's cleared and folded into `messages` once `ChatEvent::Done` arrives.

## Forms and events

Standard Dioxus idioms: `oninput: move |e| signal.set(e.value())`, `onsubmit: move |event| { event.prevent_default(); ... }`, `r#type: "submit"` (raw-identifier since `type` is a Rust keyword). Optimistic UI (showing the user's own message immediately, before the server confirms it) uses a locally-generated negative placeholder id, since real ids are always positive (`AUTOINCREMENT` starting at 1) — good enough for React/Dioxus-style list `key` uniqueness without needing the server round trip first.

## Verifying UI changes

There's no automated browser test tier yet (see [testing.md](testing.md)). Drive the running app manually with `dx serve --fullstack` and a browser, or a scripted headless Chrome session over the DevTools Protocol, for anything that touches rendering or interaction — `cargo check`/`cargo test` alone don't exercise hydration, click handlers, or the live SSE loop.
