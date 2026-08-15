# Frontend

Dioxus components under `src/frontend/`, rendered via **fullstack SSR**: the server renders real HTML for the initial page load (no blank-page-then-WASM-boot flash), then the WASM client hydrates it for interactivity.

## Structure

```
frontend/
  mod.rs           # App (router root), Route enum, Home/ConversationRoute
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

Two routes: `Home {}` at `/` (nothing selected) and `ConversationRoute { id: i64 }` at `/conversation/{id}`. Both just render `Chat {}` with no props — `Chat` derives which conversation is selected straight from the router:

```rust
let router = use_router();
let selected: Memo<Option<i64>> = use_memo(move || match router.current::<Route>() {
    Route::Home {} => None,
    Route::ConversationRoute { id } => Some(id),
});
```

This isn't just style — a plain prop threaded down from `Home`/`ConversationRoute` looked reasonable but silently breaks: switching from one conversation straight to another (still the same route *variant*, just a different `id`) re-renders the component with a new prop value without tearing down its hooks, and a `use_effect` that only reads that plain prop never re-fires (effects only re-run on a *tracked* — i.e. signal — read), so `use_resource`-driven state downstream quietly stops updating. `router.current()` is a genuine tracked read, so wrapping it in `use_memo` gives every descendant (including `use_resource`, which only restarts on a tracked read inside its own closure) a value that actually updates on navigation. Sidebar clicks and "New conversation" navigate via `use_navigator()`/`Route::ConversationRoute { id }` rather than writing to a local signal directly, so the URL stays the single source of truth — a refresh, bookmark, or direct link lands back on the same conversation because the server renders straight from the route.

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
        ChatEvent::Done { message_id, role, content } => {
            messages.write().push(Message { id: message_id, role, content, .. });
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

`src/browser_tests.rs` is a small, `#[ignore]`d automated tier (real Postgres, real k3s, real headless Chrome via CDP) covering the sandbox panel specifically — not a general framework everything else is expected to plug into yet. For anything else that touches rendering or interaction, drive the running app manually with `dx serve --fullstack` and a browser, or a scripted headless Chrome session over the DevTools Protocol — `cargo check`/`cargo test` alone don't exercise hydration, click handlers, or the live SSE loop. See [testing.md](testing.md).
