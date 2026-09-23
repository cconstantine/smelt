# Frontend

Dioxus components under `src/frontend/`, rendered via **fullstack SSR**: the server renders real HTML for the initial page load (no blank-page-then-WASM-boot flash), then the WASM client hydrates it for interactivity.

## Structure

```
frontend/
  mod.rs               # App (router root), Route enum, Home/ConversationRoute
  pages/
    mod.rs
    chat.rs            # Chat, ConversationSidebar, ChatPanel
    mcp_servers.rs      # McpServersIndex, McpServerNew, McpServerEdit
    sandbox_volumes.rs  # SandboxVolumesIndex, SandboxVolumeNew
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

The chat routes: `Home {}` at `/` (nothing selected) and `ConversationRoute { id: i64 }` at `/conversation/{id}`. Both just render `Chat {}` with no props — `Chat` derives which conversation is selected straight from the router (every other route — `/mcp-servers`, `/sandbox-volumes`, and their `new`/`:id` children — maps `None`, since `Chat` never renders there at all):

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

## The live browsing panel

`.browsing-panel` (in `ChatPanel`) is a second, independent live stream from `subscribe_conversation_events`'s — it exists only while the model has a browsing session open (`ConversationEvent::BrowsingSessionUpdate`) and subscribes separately, via `api::browsing::subscribe_browser_frames`, to a per-conversation frame channel that isn't part of `ConversationEvent` at all (frames are frequent, ephemeral, UI-only data that shouldn't crowd out task/message updates in that shared bounded broadcast channel — see `events::ConversationEvent`'s own doc comment). A dedicated `use_effect` (separate from the main event-subscription one) starts/stops that subscription as `browsing_session_open()`/`selected()` change, mirroring the main loop's own `Task`-cancel-on-change shape. If the frame stream drops (a network blip, a server restart), it reconnects after 1.5s, the same delay the main loop uses — unless `get_browsing_state` says the session is gone, in which case the panel closes.

Each frame is a base64 JPEG rendered directly as `img { src: "data:image/jpeg;base64,{data}" }` — no decoding needed (`browsing::BrowserFrame`'s own doc comment explains why: CDP's wire format is already base64, and `chromiumoxide`'s `Binary` type doesn't re-decode it). The frame element renders at a **fixed** 1280x800 CSS size matching the session's own pinned viewport exactly (`browsing::server`'s `SCREENCAST_MAX_WIDTH`/`SCREENCAST_MAX_HEIGHT`) — deliberately not responsive/scaled, so `element_coordinates()` (a mouse/wheel event's position relative to the target element) already equals real frame-pixel coordinates with no scale-factor lookup needed. `onmousemove`/`onmousedown`/`onmouseup`/`onwheel`/`onkeydown` on the frame's wrapper each build a `browsing::BrowserInputEvent` and hand it to one input queue (a `use_coroutine`), which calls `send_browser_input` one request at a time, in order. Spawning a request per event let them overtake each other: a mouse-up could reach the page before its mouse-down. Mouse moves that queue up while a request is in flight collapse to the latest one (`coalesce_mouse_moves`), so a fast-moving pointer can't build a backlog. A move carries whether the left button is actually held (`held_buttons()`, read off the real DOM event), so a plain hover reaches the page as a hover and not a drag. Only the primary button's down/up is forwarded. `onkeydown` maps a `keyboard_types::Key` to either `TypeText` (a printable character, via `Input.insertText`) or `PressKey` (a small named-key set — Enter/Backspace/Tab/Escape/Delete/arrows — via a real `Input.dispatchKeyEvent` carrying the held modifiers, so Shift+Tab and Ctrl+Backspace work, with Enter carrying its `\r` text so it submits forms) through `browser_input_event_for_key`. Wheel deltas are converted to pixels first (`wheel_delta_pixels`), since Firefox reports them in lines. Anything else (a bare modifier, an unrecognized named key, or a Ctrl/Cmd shortcut, which must not type its letter) isn't forwarded, and its default isn't prevented, so the viewer's own browser handles it as usual. Pasting into the page isn't supported.

There's no locking between the model's own tool-driven actions and the user's live input — both dispatch CDP commands against the same real page, which just serializes whichever arrives, the same as two people sharing one mouse. See [projects/completed/20260922-web-browsing.md](projects/completed/20260922-web-browsing.md) for the full design, including why this needed its own streaming channel and viewport-pinning approach.

**Coverage gap, stated plainly:** the panel's actual DOM/RSX wiring is verified manually only (a real `dx serve` + Playwright pass driving a real model through a real conversation, confirmed the panel renders live frames and that input events reach the backend correctly) — `src/browser_tests.rs`'s automated tier doesn't cover it, since automating "one headless browser watching another headless browser's live video feed" is a meaningfully bigger lift than that tier's existing scenarios. `browser_input_event_for_key`, `cdp_modifiers`, `wheel_delta_pixels` and `coalesce_mouse_moves` — the pieces of this panel's frontend logic that are pure functions rather than DOM-dependent — do have direct unit tests (`frontend::pages::chat::tests`), and the CDP mechanisms it drives (screencast frame delivery, `send_input` dispatch) are covered end-to-end at the backend level by `browsing.rs`'s own real-browser scenarios (see [testing.md](testing.md)). What's still unautomated is narrower than "the whole panel": just the RSX event handlers and the reactive frame stream themselves. Worth a real automated scenario later, not assumed away.

## Forms and events

Standard Dioxus idioms: `oninput: move |e| signal.set(e.value())`, `onsubmit: move |event| { event.prevent_default(); ... }`, `r#type: "submit"` (raw-identifier since `type` is a Rust keyword). Optimistic UI (showing the user's own message immediately, before the server confirms it) uses a locally-generated negative placeholder id, since real ids are always positive (`AUTOINCREMENT` starting at 1) — good enough for React/Dioxus-style list `key` uniqueness without needing the server round trip first.

## Verifying UI changes

`src/browser_tests.rs` is a small, `#[ignore]`d automated tier (real Postgres, real k3s, real headless Chrome via CDP) covering the sandbox panel, the context-usage indicator/detail view/compaction divider, and the todo panel — not a general framework everything else is expected to plug into yet. For anything else that touches rendering or interaction, drive the running app manually with `dx serve --fullstack` and a browser, or a scripted headless Chrome session over the DevTools Protocol — `cargo check`/`cargo test` alone don't exercise hydration, click handlers, or the live SSE loop. See [testing.md](testing.md).
