# Frontend

Dioxus components under `src/frontend/`, rendered via **fullstack SSR**: the server renders real HTML for the initial page load (no blank-page-then-WASM-boot flash), then the WASM client hydrates it for interactivity.

## Structure

```
frontend/
  mod.rs               # App (router root), Route enum, a small wrapper component per route, NotFound
  pages/
    mod.rs             # TwoStepButton, TwoStepLabel, ErrorText, server_error_message
    chat/              # the chat page (SME-57)
      mod.rs           # Chat, ChatPanel (its effects, actions and event stream)
      sidebar.rs       # ConversationSidebar
      transcript.rs    # Transcript, MessageView, render_block_element and the tool/diff/notice helpers
      context.rs       # ContextUsage: the usage bar and its detail dialog
      composer.rs      # Composer (the message box), EXAMPLE_ASKS
      repo_attach.rs   # RepoAttach ("Work on a repo")
      trust_card.rs    # TrustCards (AGENTS.md waiting for trust)
      question_card.rs # QuestionCard (the model's `ask_user` question)
      model_setup.rs   # ModelNotes (turn and notification errors)
      state.rs         # ConversationState (the store), apply_event, and the merges onto them
      sticky.rs        # use_sticky_bottom (StickyBottom) and the transcript's pointer-hold helpers
      streaming.rs     # StreamingReply (the bubble), WorkingLine and format_elapsed
      panels/          # BrowsingPanel, TodoPanel, SandboxPanel
      tests.rs         # the page's unit tests
    git.rs             # GitSettingsPage
    language_servers.rs # LanguageServersIndex, LanguageServerNew, LanguageServerEdit
    mcp_servers.rs     # McpServersIndex, McpServerNew, McpServerEdit
    pods.rs            # PodsIndex, use_pods_changed
    providers.rs       # ProvidersIndex, ProviderNew, ProviderEdit, ModelPicker (SME-72)
    sandbox_volumes.rs # SandboxVolumesIndex, SandboxVolumeNew
```

`App` mounts a viewport meta tag, a single stylesheet asset and the router:

```rust
#[component]
pub fn App() -> Element {
    rsx! {
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1" }
        // The stylesheet's parts, in order (assets/css/, SME-163).
        document::Stylesheet { href: asset!("/assets/css/01-base.css") }
        // ... one line per part, through 07-pages.css
        Router::<Route> {}
    }
}
```

Without the viewport tag a phone lays the page out at desktop width and shrinks it (SME-40 F8; browser scenario 17 checks it).

The routes (`Route` in `frontend/mod.rs`):

| Path | Page |
|---|---|
| `/` | `Chat`, nothing selected (`Home`) |
| `/conversation/:id` | `Chat` (`ConversationRoute`) |
| `/mcp-servers`, `/mcp-servers/new`, `/mcp-servers/:id` | `McpServersIndex`, `McpServerNew`, `McpServerEdit` |
| `/sandbox-volumes`, `/sandbox-volumes/new` | `SandboxVolumesIndex`, `SandboxVolumeNew` |
| `/pods` | `PodsIndex` |
| `/git` | `GitSettingsPage` |
| `/language-servers`, `/language-servers/new`, `/language-servers/:id` | `LanguageServersIndex`, `LanguageServerNew`, `LanguageServerEdit` |
| `/:..segments` (anything else, including a non-numeric conversation id) | `NotFound`: "Page not found", with a link back |

`Home {}` and `ConversationRoute { id: i64 }` both just render `Chat {}` with no props. `Chat` derives which conversation is selected straight from the router. The match needs an arm for every variant; every route other than those two maps to `None`, since `Chat` never renders there:

```rust
let router = router();
let selected: Memo<Option<i64>> = use_memo(move || match router.current::<Route>() {
    Route::Home {} => None,
    Route::ConversationRoute { id } => Some(id),
    Route::McpServersRoute {} => None,
    // ... one `None` arm per remaining variant
    Route::NotFound { .. } => None,
});
```

This isn't just style — a plain prop threaded down from `Home`/`ConversationRoute` looked reasonable but silently breaks: switching from one conversation straight to another (still the same route *variant*, just a different `id`) re-renders the component with a new prop value without tearing down its hooks, and a `use_effect` that only reads that plain prop never re-fires (effects only re-run on a *tracked* — i.e. signal — read), so `use_resource`-driven state downstream quietly stops updating. `router.current()` is a genuine tracked read, so wrapping it in `use_memo` gives every descendant (including `use_resource`, which only restarts on a tracked read inside its own closure) a value that actually updates on navigation. Sidebar clicks and "New conversation" navigate via `use_navigator()`/`Route::ConversationRoute { id }` rather than writing to a local signal directly, so the URL stays the single source of truth — a refresh, bookmark, or direct link lands back on the same conversation because the server renders straight from the route.

`Chat` also owns three `Signal<u64>` counters, passed to both `ConversationSidebar` and `ChatPanel`. Each is read inside one of the sidebar's `use_resource`s, so bumping it refetches:
- `conversations_changed` refetches `get_conversations()`. `ChatPanel` bumps it when a `MessagesAppended` event arrives and when a send finishes, which is how a new conversation's title (set by its first message) replaces "New conversation" without a reload.
- `pods_changed` refetches `get_live_pod_conversations()`, the green dot on each conversation with a live sandbox pod. `ChatPanel` bumps it on `ConversationEvent::PodsChanged`.
- `turns_changed` refetches `get_busy_conversations()`, the "Working" mark on each conversation with a turn running. `ChatPanel` bumps it on `ConversationEvent::TurnsChanged`.

`PodsChanged` and `TurnsChanged` arrive on the open conversation's own stream rather than a second connection (see [api/events.md](api/events.md#live-conversation-events)). So on `/`, with no conversation open, the dots and busy marks only refresh on navigation.

**The open conversation's state is one store** (SME-57). `ConversationState` (`chat/state.rs`, `#[derive(Store)]`) holds everything the panel shows that belongs to the open conversation: its messages and load error, the streaming reply and turn state, todos, repos and the repo form, the sandbox panel, the browsing panel and its address bar, the context meter and its detail view, the waiting question, the errors that belong to it. On a switch, `ChatPanel` replaces it in one write (the default, with `conversation` set to the new id) before subscribing to the new conversation's events, so nothing can be missed the way per-signal resets were (SME-51 B11 was the context detail view staying open over the next conversation). **Effects rerun on a switch in no set order**, so one can run before the reset, with the fields still holding the previous conversation's: an effect that acts on a field for `selected` checks `conversation` first, as the browsing panel's frame subscription does (on SME-57 (b) it asked for frames for a conversation with no session). A new per-conversation field goes in the struct, and `test_conversation_state_starts_empty` won't compile until it's checked. What outlives a switch on purpose stays outside: the message box's draft, the turn errors (kept by conversation), "Work on a repo"'s clone in flight and its error (`RepoAttaches`, kept by conversation, SME-263), the model picker, the time zone and the transcript's scroll bookkeeping.

Components take the store (`state: Store<ConversationState>`) and read the fields they show through its lenses (`state.todos()`), so a write to one field re-renders only that field's readers. **Keep what changes often in a component of its own** (SME-57 (c)): a streamed delta re-renders `StreamingReply` and the once-a-second tick `WorkingLine`, the only readers of `streaming_reply` and `turn_elapsed`; `Transcript` re-renders when the messages change, and each message is a keyed `MessageView` over memoized tool maps, so it re-renders only when its own message or a map changes. Browser scenario `streaming_into_a_long_transcript` holds this: streaming into 300 messages costs at most 3x the script time of streaming into one (it was 5.1x when every delta re-rendered the transcript). **The store lends out one write at a time:** two lenses' `write()` guards held at once panic at runtime (`AlreadyBorrowedMut`), even for different fields, since both borrow the one store. Change the sandbox pods and terminals together through `change_sandbox_panel`; anywhere else, take one guard at a time.

The sidebar links to the other pages: "MCP servers", "Sandboxes" (`/pods`), "Sandbox volumes", "Git" and "Language servers". The route and code say pods; the link and the page's heading say "Sandboxes". The Sandboxes page (`pages/pods.rs`):
- lists every live sandbox with its conversation, status, uptime, activity (busy, or idle for how long), memory and CPU use against their limits, and terminal count, plus a line under the row for its language servers, if any;
- has a Stop button per row, clicked once to arm and again to confirm;
- refetches on `AppEvent::PodsChanged` (its own `subscribe_app_events` stream, through `use_pods_changed`) and every 30 seconds;
- measures ages against the `observed_at` the server sends, not the browser's clock.

The conversation's sandbox panel has the same two-click "Stop sandbox" button.

**Each sandbox terminal's titlebar shows its last command's status** (SME-144): a pill reading `running` (its dot pulses, steady under reduced motion), `exit 0` (green), `exit N` (red) or `lost` (amber), with no pill before the first command. It sits in the titlebar, outside `.task-terminal-body`, the one part that scrolls, so it stays in view however much the command prints or however far the user scrolls up. `latest_command_indicator` (`chat/panels/sandbox.rs`) is a pure function of the terminal's last command, so the pill follows whatever the live events and the reconnect's snapshot say. `apply_sandbox_command_update` applies a finish or a `lost` update to the command with that `command_id`, not to the newest one, and an output line never changes a command's status. The pill's colours are fixed, like the rest of the terminal's dark palette, each pair at least 6.5:1; the word carries the meaning and the glyph is `aria-hidden`. The titlebar keeps one height in every state (the pills have a fixed height): the body below it scrolls, and a pixel's change there reads to the terminal's follow as a window resize. In a narrow card (container queries on `.task-terminal`; 720–1024px with a todo list beside the sandbox leaves it about 137–156px wide) the terminal's name shrinks first, the connection pill is hidden below 240px and the name below 160px, and the titlebar's tooltip still names both; the dots and the command's pill always fit. The tooltip shows the command's first line with anything in it, cut at 120 characters and isolated (U+2068/U+2069) so bidi controls in it can't reorder the status after it. The per-command status in the body's header rows stays as the history. Browser scenario `terminal_command_indicator`.

**Two-step buttons** (click once to arm, again to confirm) are `TwoStepButton` (`pages/mod.rs`): the caller keeps which one is armed, and says what arming and confirming do (`on_arm`, `on_confirm`). Its label is `TwoStepLabel`: both labels share one grid cell and the inactive one is hidden, so arming doesn't resize the button or move what's around it, and the confirming click lands where the first did. Browser scenario 12 checks this on `/pods`, and scenario 16 on the sidebar's Delete. Its clicks stop at the button, so it can sit in a row that opens on a click (the sidebar's Delete). Use it for any new two-step button. An error line is `ErrorText` (`role="alert"`, so a screen reader announces it), not a bare `p.error`.

**The sidebar's rows open from the keyboard** (SME-58): each row's title is its link and a tab stop (`role="link"`, `aria-current="page"` on the open one) that Enter or Space opens, and its Delete is a tab stop of its own, beside the link rather than inside it, where a screen reader would read it as part of the link's name (browser scenario `conversation_rows_by_keyboard`). The message box has a label for a screen reader (`.visually-hidden`), since its placeholder goes as soon as anything is typed.

**An interactive element never contains another.** No button inside a link or inside an element with `role="link"` or `role="button"`: a screen reader reads the inner control as part of the outer one's name, and some can't reach it at all. Make the part that opens it (a row's title) the link, and put the other controls beside it, as the sidebar's rows do. A plan for keyboard access names the element that takes focus. On SME-57's PR (d) the plan said "each row a tab stop that Enter opens", which led to the whole row as the link with Delete inside it; the browser scenario passed it, and only code review caught it.

**Ask the browser whether focus came from the keyboard** (`element.matches(':focus-visible')`, or the `:focus-visible` selector in CSS) rather than inferring it from pointer events. Touch, Safari and focus moved by a script each order `pointerdown`, `focus`, `pointerup` and `click` differently. On SME-105 two review rounds found outlines stuck on after a phone tap, a Safari-style click and a refused copy, all from guessing; `:focus-visible` fixed every one.

While a turn runs in the conversation (`TurnState`, whoever started it), the composer shows **Stop** next to Send. Stop calls `stop_turn`. The stop saves a notice message (`api::chat::STOP_NOTICE`), which the transcript shows as "Stopped.", so it shows in every tab and survives a reload (SME-51 B10). The turn's `TurnError` carrying `TURN_STOPPED` is ignored. If `stop_turn` itself fails, the chat shows "Couldn't stop the turn: …".

The reconnect pull (run once per stream connection, see [Streaming into the UI](#streaming-into-the-ui)) fetches messages, sandbox state, context usage, todos, repos, `get_turn_state`, `get_reply_in_progress` and `get_turn_error`, then `get_browsing_state`. Keep `get_browsing_state` last: the browser tests take that request completing as the sign the client is live.

**Connections are scarce.** Over plain HTTP/1.1, a browser allows 6 connections per host, shared by every tab. Each chat tab holds one always-open conversation stream, plus the frame stream while a browsing session is open; a `/pods` tab holds its own app-events stream. Replies stream on the conversation stream, not on the send request. Any other request (a send, a Stop click, a snapshot) needs a free connection. Don't add another always-open stream per tab; relay on the conversation stream instead, as `PodsChanged` and `TurnsChanged` do.

If `get_messages` fails with `conversation not found`, the page says "This conversation doesn't exist. It may have been deleted." and hides the message box.

## The model picker

`ModelPicker` (`pages/providers/picker.rs`) sits above the message box, keyed by conversation, and shows which model the next turn uses (`get_conversation_model`): "Provider · model", "Default: …" before the conversation has taken one, "Choose a model" when there's no default, or a link to `/providers` when there are no providers. It sets the chat panel's `model_ready` signal, which disables Send in the last two cases (and `send` checks it too). Choosing uses `ModelChooser`, a provider select plus a text field whose `<datalist>` suggests the provider's listed models, so an id can be typed when the listing fails; the same chooser sets the default on `/providers`. A choice is refetched on `ModelChanged`/`ProvidersChanged` (the `model_changed` signal). It warns when the provider says the model can't call tools, or when nothing sized its context window. It's its own row above the composer rather than inside the composer's form, so Enter in its field can't send the message.

## Calling server functions

No fetch layer to write — call the functions from `src/api/` (`chat.rs`, `browsing.rs`, `git.rs`, `language_servers.rs`, `mcp.rs`, `pods.rs`, `sandbox_volumes.rs`) directly, same as any other async function. See [api.md](api.md) for how the isomorphic call works. A typical load-on-select pattern:

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

The `use_resource` + `use_effect`-into-a-plain-signal pairing (rather than reading the resource directly in the render body) is used deliberately: it gives a stable, independently-updatable `Signal<Vec<Message>>` that `send()`'s optimistic copy and the event stream can also push into, without fighting the resource's own lifecycle.

**A restarted `use_resource` keeps its previous value until the new one lands,** and the effect that copies a value into the page runs a render after it lands. So a render that must tell a stale or not-yet-applied value from the current one tags the result with its key and checks what the value holds, as `ChatPanel`'s messages resource does with its conversation id and `shows_start_screen` with the loaded list (SME-185: a switch flashed the new-conversation start screen for both reasons).

**Showing an error:** pass a server function's error through `pages::server_error_message` (`src/frontend/pages/mod.rs`), never `e.to_string()` or `"{e}"`: `ServerFnError`'s `Display` wraps the server's message as "error running server function: … (details: None)" (SME-81). An error signal that a later action can succeed after is cleared on that success, as the sidebar's is.

**A request lives in a component that outlives it.** Dioxus cancels a component's tasks when the component unmounts, so a request `spawn`ed in a conditionally rendered child is dropped if its own result, or anything else, hides that child before it finishes, and whatever it was going to clear or set stays as it was. Spawn such requests in the page component (`ChatPanel`) and pass them down as an `EventHandler` (`on_navigate`, `on_stop_pod`, `on_attach`). On SME-57's split, the browsing panel's address bar stayed disabled after its session closed mid-navigation, until the navigation moved back to `ChatPanel`.

## Streaming into the UI

Sending is an ordinary request: `send()` adds an optimistic copy of the message (a negative id), calls `send_message`, and on failure takes that copy back by its temp id, gives the text back to a message box the viewer hasn't touched since that send, and says "Couldn't send the message: …" (`withdraw_unsent` and `draft_after_failed_send`, SME-185 B36). By id, not by text: a saved copy that has already replaced this one stays, and an in-flight copy of the same text is left alone. Everything else arrives on the conversation's event stream, the same way in every tab watching the conversation:
- `ReplyReset` starts the streaming bubble (`streaming_reply`), `ReplyDelta` adds to it, and a saved assistant message (`MessagesAppended`) or the turn ending (`TurnState { running: false }`) clears it. It's reset on a conversation switch and restored from `get_reply_in_progress` in the reconnect pull.
- `MessagesAppended` goes through `accept_saved_messages`: each saved user message replaces one optimistic copy of itself (same text), then anything new is added. Without that the sender saw its own message twice.
- `TurnError` sets the conversation's error, except for a stop (`TURN_STOPPED`), which shows as its saved notice instead (see above).
- The message box is disabled while a turn runs in the conversation (`TurnState`), whoever started it, and Stop is offered instead.

Each event goes through `apply_event(state, event)` (`chat/state.rs`), which updates the open conversation's state and returns an `EventEffect` for what lies outside it: a sidebar or model-picker counter, a turn error, or a stale bundle. The event loop in `ChatPanel` only does those. Each event has a test of its own there (`test_apply_event_*`), run against a store hosted in a `VirtualDom` (`with_state`); a new event gets one.

**A refused stream doesn't say why.** When a `ServerEvents` subscription fails (the server function returned an error before streaming), the page's side gets a transport error, not the server function's own error text, unlike an ordinary server function call. A page that needs the reason asks a plain server function: the reconnect loop calls `get_messages` when the stream fails, and stops on "conversation not found" (SME-91; its first fix matched the stream's error text and never fired).

**The model's question (SME-34).** While the conversation waits on an `ask_user` call, `QuestionCard` sits below the transcript, after any trust card: each question with its options (buttons; toggles when multi-select) and a field for the user's own answer, then Submit; a single single-choice question answers on the click. `pending_question` comes from `get_pending_question` in the reconnect pull and `QuestionUpdate` live, and is cleared on a conversation switch, so every tab shows the card and loses it together. The call itself shows nothing while it waits, and once its result arrives `render_block_element` shows it as a compact row: each question and the answer lines the model got (`answered_lines`). While a question waits the message box says "Answer the question above, or write a reply instead": sending a message dismisses it. The sidebar marks waiting conversations (`.conversation-waiting`, from `get_waiting_conversations`, refetched on `QuestionsChanged`).

Loading a conversation's messages on open goes through `apply_loaded_messages`, not a plain replace. The live subscription's reconciliation merge can land first with a message saved after the load's snapshot, and a replace would wipe it.

## Markdown in replies

SME-30. How the model's replies render as markdown (the trust boundary, code blocks, Copy reply) is in [frontend/markdown.md](<frontend/markdown.md>).

## Following the bottom

**Following the bottom** (`chat/sticky.rs`). The transcript and each sandbox terminal follow their content's bottom while the user is at it, like `tail -f`, and leave them be once they scroll up to read. Each holds a `StickyBottom` from `use_sticky_bottom()`: its element (`mounted`, from `onmounted`) and whether the user is at the bottom (`scrolled`, from `onscroll`). An effect that follows new content checks `is_stuck()`, which doesn't subscribe: `onscroll` sets it on every scroll event, and an effect that reran on each one pulled a small scroll up back down (SME-83). Each terminal is a keyed `TerminalBody` with its own, following its own output, so its scroll state goes when the terminal does. The transcript's is `ChatPanel`'s `transcript_scroll`, and only the transcript holds the text under the pointer still through a layout change (SME-75: `pointer_over_transcript`, `layout_snap_pending`, the anchor scripts). **Anything above a body that follows its bottom keeps one height in every state** (a terminal's titlebar and its pills): a terminal's follow takes a change in the body's height between two scroll events for a window resize, and a user's scroll back to the bottom then no longer resumes it. Give such an element a fixed height when its glyphs or fonts can vary: on SME-144 the command pill's glyphs came from fallback fonts and made the titlebar 1px taller when a command finished, which switched off the follow (caught only by `terminal_follow_after_switch` in the full tier).

**Sticky scroll.** An image grows its reply after the reply has rendered, so its `onload` calls `Markdown`'s `on_media_load`, which the transcript wires to the same layout-change path a side panel appearing takes (`media_loaded` in `ChatPanel`): re-snap to the bottom, or with the pointer over the transcript keep the text under it still (browser scenario `markdown_late_image`).

## The live browsing panel

`.browsing-panel` (in `ChatPanel`) is a second, independent live stream from `subscribe_conversation_events`'s — it exists only while the model has a browsing session open (`ConversationEvent::BrowsingSessionUpdate`) and subscribes separately, via `api::browsing::subscribe_browser_frames`, to a per-conversation frame channel that isn't part of `ConversationEvent` at all (frames are frequent, ephemeral, UI-only data that shouldn't crowd out task/message updates in that shared bounded broadcast channel — see `events::ConversationEvent`'s own doc comment). A dedicated `use_effect` (separate from the main event-subscription one) starts/stops that subscription as `browsing_session_open()`/`selected()` change, mirroring the main loop's own `Task`-cancel-on-change shape. If the frame stream drops (a network blip, a server restart), it reconnects after 1.5s, the same delay the main loop uses — unless `get_browsing_state` says the session is gone, in which case the panel closes.

Each frame is a base64 JPEG rendered directly as `img { src: "data:image/jpeg;base64,{data}" }` — no decoding needed (`browsing::BrowserFrame`'s own doc comment explains why: CDP's wire format is already base64, and `chromiumoxide`'s `Binary` type doesn't re-decode it). The session's viewport is pinned at 1280x800 (`browsing::server`'s `SCREENCAST_MAX_WIDTH`/`SCREENCAST_MAX_HEIGHT`), but the frame is scaled to fit the panel (`width: 100%`). A fixed 1280px frame that couldn't shrink squeezed the chat to 48px wide at laptop widths (SME-40 F2, browser scenario 15). So the wrapper tracks its shown width (`onresize`), and `frame_point(x, y, shown_width)` rescales each mouse/wheel event's `element_coordinates()` back to the page's own pixels. `onmousemove`/`onmousedown`/`onmouseup`/`onwheel`/`onkeydown` on the frame's wrapper each build a `browsing::BrowserInputEvent` and hand it to one input queue (a `use_coroutine`), which calls `send_browser_input` one request at a time, in order. Spawning a request per event let them overtake each other: a mouse-up could reach the page before its mouse-down. Mouse moves that queue up while a request is in flight collapse to the latest one (`coalesce_mouse_moves`), so a fast-moving pointer can't build a backlog. A move carries whether the left button is actually held (`held_buttons()`, read off the real DOM event), so a plain hover reaches the page as a hover and not a drag. Only the primary button's down/up is forwarded. `onkeydown` maps a `keyboard_types::Key` to either `TypeText` (a printable character, via `Input.insertText`) or `PressKey` (a small named-key set — Enter/Backspace/Tab/Escape/Delete/arrows — via a real `Input.dispatchKeyEvent` carrying the held modifiers, so Shift+Tab and Ctrl+Backspace work, with Enter carrying its `\r` text so it submits forms) through `browser_input_event_for_key`. Wheel deltas are converted to pixels first (`wheel_delta_pixels`), since Firefox reports them in lines. Anything else (a bare modifier, an unrecognized named key, or a Ctrl/Cmd shortcut, which must not type its letter) isn't forwarded, and its default isn't prevented, so the viewer's own browser handles it as usual. Pasting into the page isn't supported.

Above the frame sits an address bar showing the session's current URL, kept current by `ConversationEvent::BrowsingUrlUpdate` (seeded from `get_browsing_state`). It follows every navigation, whoever made it. While the viewer is typing in it (from focus, or the first keystroke), incoming URL changes don't overwrite their text (`address_bar_value`); Escape reverts. Enter calls `navigate_browser`, which adds a scheme when none was typed (`http://` for `localhost:5173` and other local addresses, `https://` for `example.com`; see [events.md](api/events.md)), and a failure shows under the bar (through `server_error_message`, like every error the pages show; see [Calling server functions](#calling-server-functions)).

There's no locking between the model's own tool-driven actions and the user's live input — both dispatch CDP commands against the same real page, which just serializes whichever arrives, the same as two people sharing one mouse. See [SME-22](https://linear.app/smelt-agent/issue/SME-22) for the full design, including why this needed its own streaming channel and viewport-pinning approach.

**Coverage gap, stated plainly:** the panel's input and frame wiring is verified manually only (a real app + Playwright pass driving a real model through a real conversation, confirmed the panel renders live frames and that input events reach the backend correctly). `src/browser_tests/`'s scenario 15 opens a browsing session and checks the panel's layout, but no scenario watches frames arrive or clicks on one, since automating "one headless browser watching another headless browser's live video feed" is a meaningfully bigger lift than that tier's existing scenarios. `browser_input_event_for_key`, `cdp_modifiers`, `frame_point`, `wheel_delta_pixels`, `coalesce_mouse_moves`, and `address_bar_value` — the pieces of this panel's frontend logic that are pure functions rather than DOM-dependent — do have direct unit tests (`frontend::pages::chat::tests`), and the CDP mechanisms it drives (screencast frame delivery, `send_input` dispatch) are covered end-to-end at the backend level by `browsing.rs`'s real-browser scenarios, which run inside `webfetch::browser_tests::test_fetch_scenarios` (see [testing.md](testing.md)). What's still unautomated is narrower than "the whole panel": just the RSX event handlers and the reactive frame stream themselves. Worth a real automated scenario later, not assumed away.

## Forms and events

Standard Dioxus idioms: `oninput: move |e| signal.set(e.value())`, `onsubmit: move |event| { event.prevent_default(); ... }`, `r#type: "submit"` (raw-identifier since `type` is a Rust keyword). Optimistic UI (showing the user's own message immediately, before the server confirms it) uses a locally-generated negative placeholder id, since real ids are always positive (Postgres identity columns, starting at 1) — good enough for React/Dioxus-style list `key` uniqueness without needing the server round trip first.

## Verifying UI changes

`src/browser_tests/` is an `#[ignore]`d automated tier (real Postgres, real k3s, real headless Chrome via CDP): one test running numbered scenarios in sequence, covering the side panels, live streaming across tabs, stopping a turn, the Sandboxes page, layout at laptop and phone widths, dark mode and more (list in [testing/browser-tier.md](testing/browser-tier.md#srcbrowser_tests-automated)). Add a scenario there when a change needs real-DOM verification that should keep running. For a hands-on check, start `scripts/check-server start` (a separate worktree on port 8081, so your edits don't restart it) and drive it with a browser, usually through `scripts/ui-check/smelt_ui.py` — `cargo check`/`cargo test` alone don't exercise hydration, click handlers, or the live SSE loop. See [testing/browser.md](testing/browser.md#browser-verification).
