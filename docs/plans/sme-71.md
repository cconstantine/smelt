# SME-71 plan: desktop notifications when a conversation needs the user

The plan for [SME-71](https://linear.app/smelt-agent/issue/SME-71), in a file because it's over 25,000 characters ([planner.md](../personas/planner.md)). Trade-offs and Questions for the user are in the ticket.

**Branch:** `sme-71-desktop-notifications`, worktree `~/smelt-worktrees/smelt-sme-71`, from `origin/main` at `e0cc12a` (first planned 2026-10-09 from `e177c86`).

## Revision (2026-10-17)

Checked against `main` at `e0cc12a`, 496 commits after the first plan:

1. **Paths (SME-163):** `db`, the turn tests, the browser tests, the CSS and several docs became directories (Files lists today's). The chat event loop is in `pages/chat/live.rs`, so `pages/chat/mod.rs` isn't touched.
2. **Size ceiling:** `src/turn/run.rs` is 411 bytes under it and `src/events.rs` 2,615, so two move commits come first (Files). `docs/frontend.md` (38,411) gets one link; the docs go in `docs/frontend/notifications.md`.
3. **Turn lifecycle (SME-179, SME-170, SME-160).** `TurnInFlight`'s start and end are each one critical section publishing `TurnState { running, elapsed }`, with `AppEvent::TurnsChanged` after; the attention is taken in the end's section. The generation already exists (`KeptTurnError.generation`); the plan adds a reader, `turn_generation`, not a new counter. `answer_question` is a fourth caller of `run_turn_bounded`. A refused reply gets its own kind, `Refused`. A turn racing a delete can publish for a deleted conversation; `get_attention` answers `None`.
4. **SME-162** (In Review) waits for a provider slot inside the model call: the turn stays busy; no shared files.
5. **The dev `.env` sets `SMELT_ALLOWED_HOSTS=ryzen.lan`** (the first plan said unset). Hands-on plain-HTTP tabs need `SMELT_ALLOWED_HOSTS` and can't write through `dx serve` (SME-146).
6. **opencode coordinates windows now** (`coordinator.ts`, since `c2348f8`); the comparison is rewritten, the design kept, with the reason.
7. **SME-105 shipped** the tier's `smelt-http.test` mapping, which the scenario reuses. Still no favicon, no `#[layout]`; SME-138 is Backlog; SME-73 hasn't shipped.
8. **Overlap:** SME-185 (In Progress) changes `pages/chat/state.rs` and `live.rs` too: a blocker.

## What and why

When a conversation needs the user, smelt tells them: a desktop notification from one smelt tab (on any page), and a count in each chat tab's title. "Needs the user" means the model asked a question, the turn failed, the reply was cut off or refused, or the turn finished. Clicking the notification focuses that tab and opens the conversation. The user turns notifications on with a button in the sidebar, the only place the browser's permission prompt appears. Where notifications can't work (plain HTTP such as `http://ryzen.lan:8180`, a phone browser), the button says why, and the title count still works. For real notifications from other machines, the dev Caddy can also serve `https://ryzen.lan:8443`.

Why: the user runs long tasks on a local model, and a question, failure or finished task sits unnoticed while they're elsewhere.

### The ticket's open questions, answered (the user decides, below)

- **What notifies:** a question, a failed turn, a reply cut off or refused, and a finished turn, behind one switch. A Stop never notifies: the user did it. See question 1.
  - **Question:** paused until the user answers, the ticket's reason to exist.
  - **Failed:** stopped until the user writes; includes a notice's or a wake's turn failing (`NotificationDeliveryFailed`) and `MAX_TURNS`.
  - **Cut off / Refused:** the reply stopped at its budget or was refused, and smelt doesn't continue it (`save_stop_notice` in `turn/run.rs`, SME-111, SME-122), so the user has to act.
  - **Finished:** for a long local-model task this is when the work is done. A turn a finished command starts counts too (often the real end of the work). A turn that saved no assistant message (a no-op wake, a notice saved while a question waits) doesn't. Within 30 s of a Finished notification for one conversation, another Finished for it is dropped, so a chain of quick command-notice turns doesn't buzz each time. A Question, Failed, Cut off or Refused is never held back.
  - Not notified: an error outside a turn (`show_conversation_error`, "Work on a repo" failing): the user just clicked.
- **Does a tab with no conversation open notify? Yes.** One tab at a time is the notifier. It holds the app-wide stream (`subscribe_app_events`) on any page, Home and the settings pages included. Today only `/pods` holds that stream, and Home has no stream at all. See question 2.

## How comparable tools do it

- **opencode web** (`anomalyco/opencode`, `dev` at `8c173a8`):
  - `packages/app/src/runtime/platform/web.ts`: asks for permission on the first notification, not from a click (Firefox 72+ and Safari ignore that); skips when `visibilityState === "visible" && hasFocus()`; a click does `window.focus()`, opens the session, closes it. No `tag`.
  - `shell/notifications/notification.tsx`: notifies on `session.execution.succeeded` and `…failed`, each with its own setting and sound, only for a session open in a tab.
  - `shell/notifications/coordinator.ts`: every window hears every event and claims it by id (a Web Lock named after it, then the last 500 ids in `localStorage`). A focused window holds a shared focus lock, and a notification shows only if an exclusive `ifAvailable` request for it succeeds: none while any window is focused.
  - **Not copied:** it needs every tab to hear every event, and smelt's Home and settings tabs hold no stream; one per tab costs a connection each on HTTP/1.1. One leader with one stream gives the same result for one connection per browser.
- **MDN:** [Notification()](https://developer.mozilla.org/en-US/docs/Web/API/Notification/Notification) is secure-context only and not Baseline; a new one with the same `tag` replaces the old; `renotify: true` alerts again and throws a `TypeError` without a `tag`. [LockManager.request](https://developer.mozilla.org/en-US/docs/Web/API/LockManager/request) is secure-context only; `steal: true` releases the held lock and grants the request first.
- **Web Locks spec** ([w3c.github.io/web-locks](https://w3c.github.io/web-locks/), checked 2026-10-17): a stolen lock's released promise is rejected with `AbortError` (§4.1); a document unloading terminates its locks and pending requests (§2.6).

## Which tab notifies: one leader, by Web Lock

- Every tab with notifications **on** calls `navigator.locks.request("smelt-notifier", …)` and holds the lock until its page goes away. The tab holding it is the **leader**, and only the leader notifies, so there is one notification however many tabs are open. The rest queue.
- **The leader is the tab the user last focused.** On `focus`, a tab that isn't the leader requests the lock with `steal: true`. The old leader's request rejects with `AbortError`; it drops its stream and queues again without `steal`. So the leader only checks its *own* focus to know whether the user is looking at smelt: no focus map, no messages between tabs.
- The leader's page closing or reloading passes the lock on (§2.6).
- **The leader opens one extra stream, `subscribe_app_events`,** on any page: one extra connection per browser.
- **Belt and braces:** each notification's `tag` is `smelt-conversation-<id>`, with `renotify: true`. If two leaders ever overlap (code holding a stolen lock keeps running), the second replaces the first.
- **Two origins** (`localhost:8180` and `localhost:8443` open at once) each elect a leader: locks and tags are per origin. A trade-off.
- **The notifier lives in a layout route,** so it stays mounted across navigation and can use `use_navigator`.

## What the leader does with an event

The server publishes `AppEvent::NeedsUser { conversation_id, attention, generation }` once, when a conversation goes from busy to idle with something for the user (server side below). The leader:

1. Reads the setting (`localStorage`). Off → nothing (another tab may have turned it off).
2. If this tab is visible, focused and shows that conversation (`Route::ConversationRoute { id }`), nothing: the user is looking at it.
3. For Finished, applies the 30 s cooldown per conversation, in the leader's memory.
4. Calls the new `get_attention(conversation_id, generation, attention)`, the check right before acting. It returns `None` when the event is stale:
   - a turn is running again (`turn_running`);
   - the turn generation has moved on (the user wrote or answered, or another turn took one);
   - for a question, it has been answered or dismissed;
   - the conversation is gone.

   Otherwise `AttentionDetails { title, attention, question_header }`.
5. Shows `new Notification(heading, { body, tag, renotify: true })`:
   - heading: "smelt has a question", "smelt: the turn failed", "smelt: the reply was cut off", "smelt: the model refused", or "smelt: done";
   - body: the conversation's title, plus the first question's header for a question.
6. Keeps the `Notification` in a JS map by conversation id:
   - **click:** `window.focus()`, send `open <id>` to Rust (which pushes the route), close it;
   - **revalidate:** on each `QuestionsChanged` or `TurnsChanged`, `get_attention` again for each open one; close the stale;
   - **looked at:** this tab focused on that conversation closes it.

## The server side: when a conversation "needs the user"

- `src/events.rs`:
  - `Attention { Finished, CutOff, Refused, Failed, Question }`, ordered by priority (`Ord`), serde as a string.
  - `AppEvent::NeedsUser { conversation_id: i64, attention: Attention, generation: u64 }`.
  - `ConversationEvent::NeedsUser { … }`, the same fields, relayed by `conversation_event_stream` (`api/chat.rs`) like `QuestionsChanged`, for the chat tabs' title count. A chat tab hears every conversation's.
  - An old bundle decodes both as `Unknown` (`#[serde(other)]` works on these internally tagged enums whatever fields follow) and shows SME-43's reload banner: `pods.rs`'s loop and the chat page's `apply_event` already do.
- `src/turn/state.rs` (the runtime's one `std` mutex, never across an `.await`):
  - `ConversationRuntime` gets `attention: Option<Attention>`, the most important outcome since the conversation last went busy.
  - `note_attention(id, a)` keeps the maximum, through `update_existing` (it never makes an entry: every writer runs inside a counted turn, so the entry exists). `clear_attention(id)` empties it.
  - `TurnInFlight::start`, when `turns_in_flight` goes 0 → 1, clears it in the same critical section that sets `started_at`.
  - `TurnInFlight::drop`, when the last turn ends, takes it and reads `kept.generation` in the same critical section that publishes `TurnState { running: false }`. After `AppEvent::TurnsChanged` (outside the lock), if it took one, it publishes `AppEvent::NeedsUser`.
  - `turn_generation(id) -> u64`: `kept.generation`, or 0 with no entry.
  - `is_idle` also requires `attention.is_none()`, so an entry is never dropped holding one (always true after `drop` takes it; it's there for a note racing a delete).
  - `forget_conversation_lock` (a delete) clears it.
- **Who notes what:**
  - `turn/questions.rs` `ask`, once the row is created and published: Question.
  - `turn/run.rs` `save_stop_notice`, once the notice is saved: CutOff for a cut-off reply, Refused for a refused one.
  - `run_turn_bounded` (`turn/lifecycle.rs` after the split), after its result, next to `keep_turn_error`:
    - `Err(Failed)`: Failed;
    - `Err(Stopped)`: `clear_attention` (the user is there);
    - `Ok(persisted)` with at least one `assistant` message: Finished.
  - That covers `start_turn`, `answer_question`, `deliver_notice` (via `run_turn`) and `wake_conversation`. The maximum means a later turn can't hide an earlier failure or question in the same busy spell.
- `src/api/notifications.rs` (new): `#[get("/api/conversations/{id}/attention?generation&attention")] get_attention(id: i64, generation: u64, attention: Attention) -> Option<AttentionDetails>` (query arguments as `api/providers.rs`'s `?ask_each`; the leader passes the event's kind back, since the server no longer holds it once published). It returns `None` when:
  - `turn_running(id)`;
  - `turn_generation(id) != generation`;
  - the event's kind is Question and `db::get_pending_question` has no unanswered row;
  - `db::get_conversation_title` finds no conversation.

  `db::get_conversation_title(pool, id) -> Option<String>` is new in `src/db/conversations.rs` (only `conversation_exists` exists).
- **A restart** loses the attention and generation; a turn it cuts short publishes nothing; a waiting question keeps its row (mark and card) but isn't announced again.

## Tabs, the setting, and the permission prompt

- **The setting is per browser,** `localStorage["smelt.notifications"] = "on"` (in `try`/`catch`), off by default: permission is per browser and origin, so a server-side setting would be wrong on the user's other machines.
- **The button:** in the sidebar after "Language servers", class `sandbox-volumes-link notifications-toggle`, so it looks like the links above it (`assets/css/07-pages.css`), plus a `.notifications-toggle` rule for a `button` (font, width, a disabled look) in light and dark. Its label depends on the mode:

| Mode | When | Label | Click |
| -- | -- | -- | -- |
| Unavailable | not `isSecureContext`, or no `Notification` or `navigator.locks` | "Desktop notifications need HTTPS", disabled, with a `title` naming `https://localhost:8443` and setup.md's "Dev over HTTPS" | – |
| Unsupported | `new Notification` threw (Chrome on Android) | "Notifications aren't supported in this browser", disabled | – |
| Blocked | `Notification.permission === "denied"` | "Notifications blocked in site settings", disabled | – |
| Off | otherwise, setting not on | "Turn on notifications" | asks for permission if needed, then turns on |
| On | setting on and permission granted | "Notifications on" (`aria-pressed="true"`) | turns off |

- **The permission prompt appears only from that button,** never on load or on an event.
  - The click handler calls `document::eval("return await Notification.requestPermission()")` directly, not in `spawn`; only the wait is spawned. dioxus-web 0.7.9 runs `onclick` synchronously and runs an eval's script when it's made (`src/frontend/clipboard.rs` relies on the same), so the request is inside the gesture.
  - Once granted, the setting is stored and one confirmation notification is shown ("Notifications are on"). If the constructor throws, the mode is Unsupported and the setting isn't stored.
- **Other tabs** read the mode on load, on `storage` events for that key, and on `focus`. Turning it on in one tab makes every tab request the lock; turning it off makes the leader release it and drop its stream. The leader also re-reads the setting before each notification (step 1).
- **Where it sits:** at laptop width in the sidebar's links (272 px; the longest label, about 45 characters at 13 px, wraps to two lines), above the conversation list, which moves down a row or two; it covers nothing. Below 720 px it's with the links behind the Conversations button (SME-41 D4), covering nothing when closed. Settings pages have no sidebar, so no button, but a tab there still takes part.

## When notifications can't work: the title count

- **Every chat tab, secure or not,** keeps a set of conversations that need the user, filled from the relayed `ConversationEvent::NeedsUser` while the tab isn't visible-and-focused.
  - Its title becomes `(N) smelt`, and goes back to `smelt` when the tab is focused and visible (`focus` and `visibilitychange`, through the same eval channel).
  - On each `QuestionsChanged` or `TurnsChanged` it revalidates its entries with `get_attention`, so a question answered in another tab drops out of this tab's count.
  - A leader also feeds its own count from its app stream, so a settings-page or Home leader gets one.
- **How a chat tab hands events to the notifier:** the `Shell` layout provides a context (`NotifierFeed`, two signals: the latest `NeedsUser`, and a revalidate counter). `live.rs`'s `use_conversation_events` gets one more `EventEffect` arm, `NeedsUser(…)`, which writes the first, and its existing `TurnsChanged` and `QuestionsChanged` arms also bump the second. `state.rs`'s `apply_event` maps the event to the new effect.
- **The favicon.** smelt has none (the browser's `/favicon.ico` request gets a 404). This adds `assets/favicon.svg` (an "s" in the accent colour) and `assets/favicon-dot.svg`, swapped with the title. A pinned tab shows only its favicon. See question 4.
- **No sound:** autoplay rules block it until a click, and a sound at every finished turn annoys. See question 4.
- **Settings tabs and Home over plain HTTP** have no stream, so no count. Deliberate: one stream per tab would use up HTTP/1.1's 6 connections.

## Getting real notifications from another machine

Browsers only allow them on a secure origin, which `http://ryzen.lan:8180` will never be. Two ways, both in `docs/setup.md`'s "Dev over HTTPS" (question 3):

1. **HTTPS through the dev Caddy (recommended).**
   - `docker/caddy/Caddyfile`'s site address `https://localhost:8443` becomes `{$SMELT_DEV_HTTPS_SITES:https://localhost:8443}`. Caddy substitutes them before parsing, into "multiple tokens and lines" if need be, addresses included ([Caddyfile concepts](https://caddyserver.com/docs/caddyfile/concepts), 2026-10-17).
   - `docker-compose.yml`'s `caddy` service gains `environment: - SMELT_DEV_HTTPS_SITES=${SMELT_DEV_HTTPS_SITES:-https://localhost:8443}`. The user sets `SMELT_DEV_HTTPS_SITES="https://localhost:8443, https://ryzen.lan:8443"` in `.env`.
   - `tls internal` issues the `ryzen.lan` certificate; each other machine trusts Caddy's root once (setup.md's step).
   - The dev `.env` sets `SMELT_ALLOWED_HOSTS=ryzen.lan`, so the `ryzen.lan` Host passes (the port is ignored). A secure page sends `Sec-Fetch-Site: same-origin`, so writes pass the request guard, as on `localhost:8443` today (SME-146 is about plain-HTTP names only).
   - It also fixes SME-138's clipboard and gives HTTP/2.
2. **Chrome's flag:** `chrome://flags/#unsafely-treat-insecure-origin-as-secure` with `http://ryzen.lan:8180`. Per browser, no server change; setup.md mentions it as the quick way.

Production (`*.constantlee.us`) is HTTPS already.

## State model

### 1. A conversation's attention (server, in memory)

The record is `ConversationRuntime.attention`; the generation is `kept.generation`, read when it's published.

| State \ event | `ask` | cut-off / refusal notice saved | run ends Failed | run ends Ok with an assistant message | run ends Ok, none | Stop | last turn ends | a turn starts (from idle) | user sends or answers | conversation deleted | restart |
| -- | -- | -- | -- | -- | -- | -- | -- | -- | -- | -- | -- |
| idle, none | – | – | – | – | – | – | – | → busy, none | generation +1 | – | – |
| busy, none | → Question | → CutOff / Refused | → Failed | → Finished | stays | stays none | → idle, nothing published | (already busy) | generation +1 | cleared | lost |
| busy, X | max(X, Question) | max(X, …) | max(X, Failed) | max(X, Finished) | stays X | → none | → idle; publish `NeedsUser(X, generation)` | – | generation +1 (the event will be stale) | cleared (a later note can set it again; see below) | lost |

**When the value is taken or cleared, per kind of caller** (it's taken only when the *last* turn in flight ends; this is what that turn leaves for it):

| Caller \ ends | Stopped while queued | Conversation not found | Failed before saving its input | Failed after | Nothing to do | Done |
| -- | -- | -- | -- | -- | -- | -- |
| user's message (`start_turn`) | cleared (message saved, SME-170) | Failed noted; `get_attention` → `None` | Failed | Failed | – (always has a message) | Finished, or Question if it asked |
| answer (`answer_question`) | cleared (answer left for the next turn) | as above | Failed | Failed | unchanged (answer taken by another turn) | Finished / Question |
| notice (`deliver_notice`) | cleared | as above | Failed | Failed | – | Finished / Question; nothing if a question waits (no reply) |
| wake (`wake_conversation`) | cleared | as above | – (no input) | Failed | unchanged (no-op wake) | Finished / Question |

A Stop clearing the value also clears an earlier turn's Failed or Question in the same busy spell: the user pressed Stop, so they're looking.

**Writers:** `ask` (Question); `save_stop_notice` (CutOff, Refused); `run_turn_bounded`'s ending (Failed, Finished, the clear on Stop), for all four callers; `TurnInFlight::start` (clear) and `drop` (take and publish); `forget_conversation_lock` (clear). All under the runtime mutex.

**Readers:** `TurnInFlight::drop` (takes it). `get_attention` reads `turn_running`, `turn_generation`, the question row and the title, not the attention field.

**Failure part-way:**

- `ask`'s row insert fails: nothing noted; the turn fails, so Failed.
- The cut-off notice's save fails: nothing noted yet (the note follows the save); the turn errors, so Failed.
- Publishing can't fail (a `broadcast` send with no receivers is fine).

**A delete racing a turn.** `forget_conversation_lock` clears the attention, but the running turn then notes Failed ("conversation not found", or a save failing), and its end publishes `NeedsUser` for a deleted conversation. `get_attention` finds no title and answers `None`, so nothing shows, and chat tabs drop it on their next revalidation. A server guard would need the turn's lock handle in `run_turn_bounded`; not worth one wasted request.

**Between the publish and the check:** a new turn can start right after `drop`'s critical section and before `NeedsUser` is sent (the runtime lock is released between). The leader then hears `TurnsChanged`, then `NeedsUser`, and `get_attention` sees `turn_running` → `None`.

### 2. The leader lock (each tab, in the browser)

| State \ event | setting turned on | focus | lock stolen | page closes or reloads | setting turned off | stream drops |
| -- | -- | -- | -- | -- | -- | -- |
| off | → queued (plain request) | – | – | – | – | – |
| queued | – | steal → leader | – | request aborted (§2.6) | abort the request (an `AbortSignal`) → off | – |
| leader | – | – | `AbortError` → drop the stream, queue again | lock released → the next queued tab leads | release → off | reconnect after 1.5 s, like `use_pods_changed` |

### 3. A shown notification (in the tab that made it)

- **States:** shown, then closed.
- **It closes when:** the user clicks it (the conversation opens in that tab); a revalidation finds it stale (answered elsewhere, a new turn, the conversation deleted); that tab shows the conversation with focus; a new one with the same tag replaces it; the user dismisses it; the page goes away (non-persistent notifications live with their page).
- **A demoted leader keeps the notifications it showed.** As a chat tab it still hears the relayed `QuestionsChanged` and `TurnsChanged` and revalidates. As a settings tab or Home it doesn't, so a stale one stays until clicked or dismissed; clicking it just opens the conversation.

**Between the check and the act:**

- *Between `get_attention` and `new Notification`* another tab can answer, or a turn start; each publishes `QuestionsChanged` or `TurnsChanged`, and the leader revalidates and closes it: shown for at most one round trip.
- *A question answered before the event reaches the leader:* `get_attention` → `None`, nothing shows.
- *The user switches smelt tabs while the leader decides:* the new tab steals the lock and an event in flight on the old stream is lost; the user is in smelt then. Likewise during a 1.5 s reconnect or a hand-over.
- *The setting turned off between the event and the show:* step 1 reads it right before showing, and turning it off also releases the lock.

**A tab reloaded:** its lock passes on; nothing is re-announced (no catch-up on connect, deliberately: a waiting question shows as the sidebar's `?` and the card). Its notifications close with the page.

**Who sees each change:** the event reaches every tab with a stream (the leader by `subscribe_app_events`, chat tabs relayed). Only the leader notifies; each chat tab keeps its own title count.

**Work already in flight:**

- The setting is read right before each notification, so turning it off stops the next one. A notification already shown stays until a close path above.
- A turn waiting for a provider slot (SME-162, once merged) is in flight, so the conversation stays busy and nothing is published until the turn ends; a Stop while it waits clears the attention as any Stop does.

**What each caller does in each mode** (the notifier, per tab):

| Tab \ mode | Unavailable | Unsupported | Blocked | Off | On |
| -- | -- | -- | -- | -- | -- |
| chat tab, not leader | title count only | title count only | title count only | title count only | title count; queued; steals on focus |
| any page, leader | – | – | – | – | app stream; notifies; title count |
| Home or settings page, not leader | nothing | nothing | nothing | nothing | queued; steals on focus |
| the sidebar button | disabled, says HTTPS | disabled | disabled | asks, then on | turns off |

## Files

Sizes are now, against the 40,000-byte ceiling (30,000 aim).

**Commits 1 and 2: moves only,** each checked by a build of every target and `scripts/check.sh`:

1. `src/turn/run.rs` (39,589): `run_turn`, `record_saved`, `run_turn_bounded`, `run_turn_stoppable`, `relay_reply_delta`, `end_stopped_turn`, `record_stop` and `start_turn` (lines 51–360, about 14,400 bytes) move to a new `src/turn/lifecycle.rs`, declared in `src/turn/mod.rs` with `pub(crate) use self::lifecycle::*;`. `run.rs` goes to about 25,200.
2. `src/events.rs` (37,385): `mod server`'s `mod tests` (about 12,400 bytes) moves to `src/events/server/tests.rs` and `mod wire_tests` (about 7,200) to `src/events/wire_tests.rs`, each declared `#[cfg(test)] mod …;` where it was. `events.rs` goes to about 17,800.

**New:**

- `src/api/notifications.rs`: `get_attention`, `AttentionDetails`, tests.
- `src/turn/tests/attention.rs`: the server tests (one `mod attention;` line in `src/turn/tests/mod.rs`, 12,648).
- `src/frontend/notifications.rs`: the `Shell` layout component; the `Notifier` (eval channel, lock, the leader's stream, the title count); `NotifierFeed`; the `NotificationsToggle` button; the JS as `const` strings; pure functions for unit tests: `notify_mode`, `should_notify` (setting, focus, route, cooldown), `notification_text`, `badge_title`. Aim under 30,000; the unit tests go in `src/frontend/notifications/tests.rs` from the start.
- `src/browser_tests/notifications.rs`: the scenario (plus its `mod` line and one `run_scenario` line in `browser_tests/mod.rs`).
- `assets/favicon.svg` and `assets/favicon-dot.svg`, if question 4 says yes.
- `docs/frontend/notifications.md` (the notifier, the layout route, the button, the title count), linked from `docs/frontend.md` (38,411; one line).

**Changed:**

- `src/events.rs` (17,800 after the move): `Attention`, both `NeedsUser` variants; wire tests in `events/wire_tests.rs`.
- `src/api/mod.rs`: one `pub mod notifications;` line.
- `src/api/chat.rs` (20,282): the relay arm in `conversation_event_stream`, and its test.
- `src/turn/state.rs` (18,764): the attention field, `note_attention`, `clear_attention`, `turn_generation`, `TurnInFlight` start and drop, `is_idle`, `forget_conversation_lock`.
- `src/turn/lifecycle.rs` (about 14,400 after the move): the notes in `run_turn_bounded`.
- `src/turn/run.rs` (about 25,200 after the move): the notes in `save_stop_notice`.
- `src/turn/questions.rs` (6,646): the note in `ask`.
- `src/db/conversations.rs` (7,895): `get_conversation_title`; its test in `src/db/tests/conversations.rs` (10,421).
- `src/frontend/mod.rs` (6,248): `#[layout(Shell)]` around every route and `#[end_layout]`, so `Shell` (rendering `Notifier {}` and `Outlet::<Route> {}`) is inside the router on every page; `mod notifications;`; the favicon `document::Link`.
- `src/frontend/pages/chat/state.rs` (26,170): `EventEffect::NeedsUser` and its `apply_event` arm; a test in `chat/tests/events.rs` (15,121).
- `src/frontend/pages/chat/live.rs` (14,383): the `NeedsUser` arm, and the `NotifierFeed` bump in the `TurnsChanged` and `QuestionsChanged` arms.
- `src/frontend/pages/chat/sidebar.rs` (10,447): `NotificationsToggle {}` after the Language servers link.
- `src/frontend/pages/pods.rs`: nothing; its `_ => {}` already skips the new `AppEvent`. The Developer confirms.
- `assets/css/07-pages.css` (18,909): `.notifications-toggle`, light and dark, next to `.sandbox-volumes-link`.
- `docker/caddy/Caddyfile`, `docker-compose.yml` (10,561; the `caddy` service's `environment`), if question 3 says yes.
- Docs: `docs/setup.md` (23,930; "Dev over HTTPS": other hosts, the Chrome flag, why notifications need it), `docs/api/events.md` (26,471; `NeedsUser`, `Attention`), `docs/api.md` (29,137; `get_attention`), `docs/architecture/modules-core.md` (13,517; `api/notifications.rs`, `frontend/notifications.rs`, `turn/lifecycle.rs`), `docs/testing/browser-tier.md` (19,543; the scenario, the Notification stub).
- At close-out: `docs/current-state/features.md` (34,231; one bullet), `docs/current-state/architecture.md` (the events line), and a Feature checklist row (patch edit, read back).

**Not touched:** `pages/chat/mod.rs`, `transcript.rs`, `clipboard.rs`, `turn/notify.rs`, `src/sandbox/*`, the agent, `Cargo.toml`/`Cargo.lock` (no new crate; browser APIs go through `document::eval`). No image rename.

Tests that reach `TurnInFlight` and `run_turn_bounded` (every `src/turn/tests/*` file, `browser_tests/{basics,notices}.rs`) need no new row or server: the attention is in memory.

### Overlap

Against Planned, Todo, In Progress, In Review and Approved, with PRs #144, #143, #142 and #139's diffs:

- **SME-185 (In Progress, PR #144) overlaps:** it changes `pages/chat/state.rs` (`ConversationState`) and `live.rs` (`use_initial_messages`), as this plan does (`EventEffect`, `apply_event`, `use_conversation_events`). The hunks are apart, but it's a blocker.
- No shared files with SME-189, SME-184, SME-162, SME-186 or SME-187. SME-123 has no file list yet.
- SME-79 (Todo) changes `turn/notify.rs` and adds turn tests; this plan doesn't touch `notify.rs` and its tests go in a new file. Not a blocker. **But** its idle notices wake a forgotten command's conversation every 5 minutes, and each such turn would notify as Finished (question 1).

## Tests (each seen failing first)

Predictions of a red are marked "expected" and are predictions; the Developer records the actual red next to them.

### Server unit and mock-upstream tests

In `turn/tests/attention.rs`, `api/notifications.rs`, `events/wire_tests.rs`, `api/chat.rs`. Ids from `db::test_support::start_ids_clear_of_other_runs` (`docs/testing.md`, "Tests that touch per-conversation state"); the app channel is shared by every test, so each counts only its own conversation's `NeedsUser`.

- One `NeedsUser` when the conversation goes idle, for each outcome:
  - `ask_user` → Question;
  - a cut-off reply (mock `stop_reason: max_tokens`) → CutOff; a refusal (`stop_reason: refusal`) → Refused;
  - an upstream 500 → Failed; a notice's turn (`deliver_notice`) failing → Failed; a wake failing → Failed;
  - a plain reply → Finished, for each of `start_turn`, `answer_question`, `deliver_notice` and a wake with a pending command notice.
- Nothing published for: a Stop mid-turn; a Stop catching a queued message (SME-170 path); a no-op wake; a notice saved while a question waits.
- A queued notice turn that finishes after a failed run publishes Failed (the maximum), once.
- A Stop after a Question in the same busy spell publishes nothing.
- `NeedsUser` comes after `AppEvent::TurnsChanged` on the app channel, and after `ConversationEvent::TurnState { running: false }` on the conversation's, and carries `turn_generation` as it was at that end.
- The runtime entry is still freed when nothing else holds it (`has_turn_state` false after a delete and a published outcome).
- `turn_generation`: 0 with no entry; follows `new_turn_generation` and `take_turn_generation`.
- `get_attention`:
  - `Some` for a fresh event, with the title and, for a question, its first header;
  - `None` for a stale generation (`start_turn` since);
  - `None` while a turn runs;
  - `None` for a question answered, and for one dismissed by the user's message;
  - `None` for a deleted conversation (including the delete-racing-a-turn case: delete while the mock upstream holds the turn, then release it).
- The wire round trip of `Attention` and both variants; an unknown `type` still decodes as `Unknown`.
- The relay arm in `conversation_event_stream` (a channel of the test's own, as SME-135's tests do).

Which part each test needs: the outcome tests need the notes *and* `drop`'s publish (with only the notes, nothing is published: expected red "no NeedsUser within 5 s"); the "Failed wins" test needs `note_attention`'s maximum (with a plain overwrite it sees Finished); the "Stop after Question" test needs the clear on `Err(Stopped)`; the ordering test needs the publish after `TurnsChanged` (publishing inside the critical section, before it, fails it). The Developer stubs each part once and sees its test go red (tdd.md, "Check that a fixture set bites").

### Frontend unit tests

In `src/frontend/notifications/tests.rs`, plain functions: `notify_mode` for every row of the mode table; `should_notify` (setting off, focused on that conversation, focused on another, hidden, the cooldown for Finished only); `notification_text` for the five kinds; `badge_title` for 0, 1 and N. In `chat/tests/events.rs`: `apply_event` maps `NeedsUser` to `EventEffect::NeedsUser`.

### Browser tier (new scenario `desktop_notifications`)

It runs on `127.0.0.1` with chrome-headless-shell, a secure context.

- **Spike first:** that headless shell has `navigator.locks`, and `focus` and `steal` behave as the plan needs. If `focus` doesn't fire between headless tabs, the scenario forces leadership by dispatching `focus` itself and leaves the real focus path to the hands-on check.
- **The stub** (`evaluate_on_new_document`, each tab): `window.Notification` with `permission: "granted"`, `requestPermission` → `"granted"`, each construction (heading, body, tag, renotify) recorded in `window.__notifications` with `close()` and a callable `onclick`.

1. Tab A on conversation C1, tab B on `/providers`. Click the button in A: it reads "Notifications on", and `localStorage` holds the setting.
2. A mock upstream (as `ask_user_card`'s) makes C2's turn call `ask_user`. Across both tabs exactly **one** notification is recorded, with tag `smelt-conversation-C2` and C2's title in the body.
3. Calling its `onclick` moves that tab to `/conversation/C2`.
4. Another C2 question; answer it on the card in the other tab: the recorded notification's `close()` was called.
5. Reload whichever tab leads. A finished turn in C1 still gives exactly one notification, from one tab, and the reload didn't re-announce C2.
6. Turn the setting off: the next finished turn records nothing.
7. **Plain HTTP:** a tab at `http://smelt-http.test:<port>/conversation/C1` (the tier's existing `INSECURE_HOST` mapping), where `isSecureContext` is false.
   - The button reads "Desktop notifications need HTTPS" and is disabled.
   - No `Notification` is constructed.
   - After a finished turn in C1 (started from a `127.0.0.1` tab, since writes from the plain-HTTP name can be refused, SME-146) while this tab is unfocused, `document.title` is `(1) smelt`, and back to `smelt` once it has focus (forced by dispatching `focus` and `visibilitychange` if headless won't move focus).
8. At 390 px the button sits inside the opened sidebar list, with no sideways scroll; at 721 px, with the widest label, it wraps inside the 272 px sidebar without overflowing.

Each check is run once with its piece of the feature switched off, and must fail.

### Hands-on check

From the hands-on template in `docs/personas/rules.md`, as `<scratchpad>/sme-71/hands-on.sh`: `CHECK_SCRATCH_DB=1`, `CHECK_WORKTREE=~/smelt-worktrees/check-sme-71` (this worktree contains SME-115's fix), started with `-- SMELT_ALLOWED_HOSTS=smelt-http.test,ryzen.lan` so the plain-HTTP tab's reads pass. No cluster lock (SME-100).

- A mock Anthropic-compatible provider added in the UI calls `ask_user` first, then answers plainly (as SME-34's hands-on).
- A Playwright script on `scripts/ui-check/smelt_ui.py`'s `run`, in Chromium with `--host-resolver-rules=MAP smelt-http.test 127.0.0.1`, checks with the stub: leader election across two real tabs with real focus changes (`page.bring_to_front()`), the click opening the conversation, the plain-HTTP button and title count (messages sent from the `localhost` tab, SME-146), and a leader left in the background a few minutes (tab freezing). Screenshots of the sidebar at 1400 and 390 px, light and dark. `run` deletes its conversations before `stop`.

### After the merge (for the user, in What shipped)

- In a real Chrome on `https://localhost:8443`, click "Turn on notifications". Switch to another app, send a long task, check that one desktop notification arrives and that clicking it opens the conversation.
- Then, with `SMELT_DEV_HTTPS_SITES` set and `docker compose up -d caddy`, the same from another machine at `https://ryzen.lan:8443`.
