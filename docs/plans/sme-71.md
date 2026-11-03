# SME-71 plan: desktop notifications when a conversation needs the user

The plan for [SME-71](https://linear.app/smelt-agent/issue/SME-71), in a file because it's over 25,000 characters ([planner.md](../personas/planner.md)). Trade-offs are in the ticket; the first plan's four questions were answered (2026-11-03) and are decisions here.

**Branch:** `sme-71-desktop-notifications`, from `origin/main` at `0958eee` (first planned 2026-10-09 from `e177c86`; revised 2026-10-17 against `e0cc12a`; this round rebuilt that branch over `0958eee`).

## Revision (2026-11-03)

Checked against `main` at `0958eee` (SME-73's merge). What moved since `e0cc12a` and why:

1. **SME-73 merged.** An overflowing turn now learns the window the refusal named, compacts and sends again (`turn/overflow.rs`), so the old plan's "an overflowing turn still notifies as Failed" is gone: a turn fails — and notifies Failed — only on a **second** overflow refusal, a window too small for smelt's prompt and tools, a failed model call, `MAX_TURNS`, or any other mid-turn failure. A user turn that fails with its failure notice saved ends as `TurnFailure::Noted` (SME-123) — Failed here too. A reply the *window* stopped gets the same notice as a budget one (SME-73) — one kind, `CutOff`.
2. **Turn files moved.** `save_stop_notice` left `run.rs` for `turn/reply.rs` (16,876), so the CutOff/Refused note goes there. `run.rs` shrank to 38,109, still inside the ~2,000 split band and still home to `run_turn_bounded`, which gains a note — so the lifecycle split stays the plan's one move-only commit (Files).
3. **`src/events.rs` became `src/events/`** (`mod.rs` 12,309, `server/tests.rs` 10,020, `wire_tests.rs` 7,619): the old plan's second move commit is done upstream and drops. `Attention` and both `NeedsUser` variants go in `mod.rs`; wire tests in `wire_tests.rs`.
4. **SME-185 is Done** — the blocker this plan carried is gone; `pages/chat/state.rs` (27,416), `live.rs` (18,415) and `chat/tests/events.rs` (16,286) re-measured. `sidebar.rs` became `sidebar/mod.rs` (23,907) + `tree.rs`; the toggle lands in `sidebar/mod.rs`.
5. **SME-79 is Done** (idle notices shipped: `turn/notify.rs`'s drain). Each idle-notice turn saves an assistant reply, so each notifies Finished — now a live trade-off, and dropping them is its own ticket.
6. **`ask_parent` (SME-236/238) waits with a `pending_questions` row too** (`asked_of = parent`), one no card can answer. The Question note stays exactly on `ask` (the `ask_user` path), and `get_attention`'s question check goes through `turn::questions::pending_question`, which answers nothing for a parent row: a child waiting on its parent never notifies.
7. **The four questions are answered** (2026-11-03; decided facts below): all five kinds notify behind one switch, 30 s per-conversation Finished cooldown, a Stop never notifies; a tab with no conversation open still notifies; **no dev-Caddy change and no ticket for one** — the `SMELT_DEV_HTTPS_SITES` idea, the `docker/caddy/Caddyfile` and `docker-compose.yml` edits and the setup.md "other hosts" paragraph are out; the title count keeps its favicons (`assets/favicon.svg`, `assets/favicon-dot.svg`), no sound.
8. **Two gates landed.** `scripts/check.sh` now fails on a test-build warning (SME-361): the move commit and the attention code must compile clean in every build, dead code included. A repo lint fails code comments citing a missing `.rs` path (SME-340): every path this plan cites exists at `0958eee` or arrives with an earlier commit.
9. **Overlap: no blockers.** The only other ticket with a plan is SME-229 (In Progress, PR #236, the only open PR): all `scripts/`, `src/bin/sandbox_image_import*` and docs; its `docs/setup.md` edit is sections away from this plan's "Dev over HTTPS" paragraph. `pages/pods.rs` still skips unknown `AppEvent`s (`use_pods_changed`'s `_ => {}`) after SME-304's Node column, so it stays untouched.

## What and why

When a conversation needs the user, smelt tells them: a desktop notification from one smelt tab (on any page), and a count in each chat tab's title. "Needs the user" means the model asked a question, the turn failed, the reply was cut off or refused, or the turn finished. Clicking the notification focuses that tab and opens the conversation. The user turns notifications on with a button in the sidebar, the only place the browser's permission prompt appears. Where notifications can't work (plain HTTP such as `http://ryzen.lan:8180`, a phone browser), the button says why, and the title count still works.

Why: the user runs long tasks on a local model, and a question, failure or finished task sits unnoticed while they're elsewhere.

### What notifies (decided 2026-11-03)

A question, a failed turn, a reply cut off or refused, and a finished turn, behind one switch. A Stop never notifies: the user did it.

- **Question:** paused until the user answers, the ticket's reason to exist. The `ask_user` path only — an `ask_parent` wait is between agents, never notified (Revision 6).
- **Failed:** stopped until the user writes; includes a notice's or a wake's turn failing (`NotificationDeliveryFailed`) and `MAX_TURNS`, and a user turn that failed with its failure notice saved (`TurnFailure::Noted`).
- **Cut off / Refused:** the reply stopped at its budget or its window, or was refused, and smelt doesn't continue it (`save_stop_notice`, `turn/reply.rs`: SME-111, SME-122, SME-73): the user has to act.
- **Finished:** for a long local-model task this is when the work is done. A turn a finished command starts counts too (often the real end of the work). A turn that saved no assistant message (a no-op wake, a notice saved while a question waits) doesn't. A second Finished for one conversation within 30 s is dropped, so a chain of quick command-notice turns doesn't buzz each time; Question, Failed, Cut off and Refused are never held back.
- Not notified: an error outside a turn (`show_conversation_error`, "Work on a repo" failing): the user just clicked.

**A tab with no conversation open notifies** (Home, the settings pages — decided). Today only `/pods` holds the app-wide stream (`subscribe_app_events`), and Home has none.

## How comparable tools do it

- **opencode web** (`anomalyco/opencode`, `dev` at `8c173a8`):
  - `packages/app/src/runtime/platform/web.ts`: asks for permission on the first notification, not from a click; skips when `visibilityState === "visible" && hasFocus()`; a click does `window.focus()`, opens the session, closes it; no `tag`. It notifies on `session.execution.succeeded`/`…failed` (`shell/notifications/notification.tsx`), each with its own setting and sound, only for a session open in a tab.
  - `shell/notifications/coordinator.ts`: every window hears every event and claims it by id (a Web Lock per id, plus the last 500 ids in `localStorage`); a notification shows only if an exclusive `ifAvailable` focus lock succeeds — none while any window is focused.
  - **Not copied:** it needs every tab to hear every event; smelt's Home and settings tabs hold no stream, and one each would eat HTTP/1.1's 6 connections. One leader with one stream gives the same result for one connection per browser.
- **MDN:** [Notification()](https://developer.mozilla.org/en-US/docs/Web/API/Notification/Notification) is secure-context only, not Baseline; same `tag` replaces; `renotify: true` re-alerts and throws without a tag. [LockManager.request](https://developer.mozilla.org/en-US/docs/Web/API/LockManager/request) is secure-context only; `steal: true` grants first, releasing the holder.
- **Web Locks spec** ([w3c.github.io/web-locks](https://w3c.github.io/web-locks/)): a stolen lock's release rejects with `AbortError` (§4.1); an unloading document's locks and pending requests end (§2.6).

## Which tab notifies: one leader, by Web Lock

- Every tab with notifications **on** calls `navigator.locks.request("smelt-notifier", …)` and holds the lock until its page goes away. The holder is the **leader**; only the leader notifies — one notification however many tabs are open. The rest queue.
- **The leader is the tab the user last focused.** On `focus`, a tab that isn't the leader requests the lock with `steal: true`. The old leader's request rejects with `AbortError`; it drops its stream and queues again without `steal`. So the leader checks only *its own* focus: no focus map, no messages between tabs.
- The leader's page closing or reloading passes the lock on (§2.6).
- **The leader opens one extra stream, `subscribe_app_events`,** on any page: one extra connection per browser.
- **Belt and braces:** each notification's `tag` is `smelt-conversation-<id>` with `renotify: true`: if two leaders ever overlap (stolen-lock code keeps running), the second replaces the first.
- **Two origins** (`localhost:8180` + `localhost:8443` open at once) each elect a leader: locks and tags are per origin. A trade-off.
- **The notifier lives in a layout route,** so it stays mounted across navigation and can use `use_navigator`.

## What the leader does with an event

The server publishes `AppEvent::NeedsUser { conversation_id, attention, generation }` once, when a conversation goes busy→idle with something for the user. The leader:

1. Reads the setting (`localStorage`). Off → nothing (another tab may have turned it off).
2. If this tab is visible, focused and shows that conversation (`Route::ConversationRoute { id }`), nothing: the user is looking at it.
3. For Finished, applies the 30 s cooldown per conversation, in the leader's memory.
4. Calls the new `get_attention(conversation_id, generation, attention)`, the check right before acting. It returns `None` when the event is stale:
   - a turn is running again (`turn_running`);
   - the turn generation has moved on (the user wrote or answered, or another turn took one);
   - for a question, it has been answered or dismissed, or the waiting row is `ask_parent`'s;
   - the conversation is gone.

   Otherwise `AttentionDetails { title, attention, question_header }`.
5. Shows `new Notification(heading, { body, tag, renotify: true })`:
   - heading: "smelt has a question", "smelt: the turn failed", "smelt: the reply was cut off"/"smelt: the model refused", or "smelt: done";
   - body: the conversation's title, plus the first question's header for a question.
6. Keeps the `Notification` in a JS map by conversation id:
   - **click:** `window.focus()`, send `open <id>` to Rust (which pushes the route), close it;
   - **revalidate:** on each `QuestionsChanged` or `TurnsChanged`, `get_attention` again for each open one; close the stale;
   - **looked at:** this tab focused on that conversation closes it.

## The server side: when a conversation "needs the user"

- `src/events/mod.rs` (12,309):
  - `Attention { Finished, CutOff, Refused, Failed, Question }`, ordered by priority (`Ord`), serde as a string.
  - `AppEvent::NeedsUser { conversation_id: i64, attention: Attention, generation: u64 }`.
  - `ConversationEvent::NeedsUser { … }`, the same fields, relayed by `conversation_relay` (`api/chat.rs`) like `QuestionsChanged`, for the chat tabs' title count. A chat tab hears every conversation's.
  - An old bundle decodes both as `Unknown` (`#[serde(other)]` works on internally tagged enums whatever fields follow) and shows SME-43's reload banner: `pods.rs`'s loop and the chat page's `apply_event` already do.
- `src/turn/state.rs` (27,754; the runtime's one `std` mutex, never across an `.await`):
  - `ConversationRuntime` gets `attention: Option<Attention>`, the most important outcome since the conversation last went busy.
  - `note_attention(id, a)` keeps the maximum, via `update_existing` (never creates an entry: writers run inside a counted turn, so it exists). `clear_attention(id)` empties it.
  - `admit` (shared by `TurnInFlight::start_by_user` and `start_unless_paused`) clears it on 0 → 1, in the critical section that sets `started_at`.
  - `TurnInFlight::drop`, when the last turn ends, takes it and reads `kept.generation` in the same critical section that publishes `TurnState { running: false }`. After `AppEvent::TurnsChanged` (outside the lock), if it took one, it publishes `AppEvent::NeedsUser`.
  - `turn_generation(id) -> u64`: `kept.generation`, or 0 with no entry.
  - `is_idle` also requires `attention.is_none()`, so an entry is never dropped holding one (guards a note racing a delete).
  - `forget_conversation_lock` (a delete) clears it.
- **Who notes what:**
  - `turn/questions.rs` `ask`, once the row is created and published: Question. The `ask_user` path only — `agents::ask_parent` records its row through `db::ask_parent` and notes nothing (Revision 6).
  - `turn/reply.rs` `save_stop_notice`, once the notice is saved: CutOff for a cut-off reply (budget or window), Refused for a refused one.
  - `run_turn_bounded` (`turn/lifecycle.rs` after the split), after its result, next to `keep_turn_error` and before `drop(in_flight)` (which is what takes the attention):
    - `Err(TurnFailure::Failed(_))` and `Err(TurnFailure::Noted { .. })`: Failed;
    - `Err(TurnFailure::Stopped)`: `clear_attention` (the user is there);
    - `Ok(persisted)` with at least one `assistant` message, and no question waiting: Finished — *wait-aware (2026-11-03)*: when `question_waits` the turn ended on a wait (a user row noted its Question at `ask`; parent rows notify nothing), so no Finished too.
  - That covers `start_turn` (`run.rs`), `answer_question` (`questions.rs`), `deliver_notice` and `wake_conversation` (`notify.rs`). A wake with no pending work is never counted (`wake_has_work`, SME-218), so it notes and publishes nothing. The maximum means a later turn can't hide an earlier failure or question in the same busy spell.
- `src/api/notifications.rs` (new): `#[get("/api/conversations/{id}/attention?generation&attention")] get_attention(id: i64, generation: u64, attention: Attention) -> Option<AttentionDetails>` (query arguments as `api/providers.rs`'s `?ask_each`; the leader passes the event's kind back, since the server no longer holds it once published). It returns `None` when:
  - `turn_running(id)`;
  - `turn_generation(id) != generation`;
  - Question and `turn::questions::pending_question` (filters answered and parent rows) has nothing;
  - `db::get_conversation_title` finds no conversation.

  `db::get_conversation_title(pool, id) -> Option<String>` is new in `src/db/conversations.rs` (only `conversation_exists` exists).
- **A restart** loses the attention and generation; a turn it cuts short publishes nothing; a waiting question keeps its row (mark and card) but isn't announced again.

## Tabs, the setting, and the permission prompt

- **The setting is per browser,** `localStorage["smelt.notifications"] = "on"` (in `try`/`catch`), off by default: permission is per browser and origin, so a server-side setting would be wrong on the user's other machines.
- **The button:** in the sidebar after "Language servers" (`sidebar/mod.rs`, `nav.settings-links`), class `sandbox-volumes-link notifications-toggle`, so it looks like the links above it (`assets/css/07-pages.css`, whose `.settings-link` rule is at lines 14–22), plus a `.notifications-toggle` rule for a `button` (font, width, a disabled look) in light and dark. Its label depends on the mode:

| Mode | When | Label | Click |
| -- | -- | -- | -- |
| Unavailable | not `isSecureContext`, or no `Notification` or `navigator.locks` | "Desktop notifications need HTTPS", disabled, with a `title` naming `https://localhost:8443` | – |
| Unsupported | `new Notification` threw (Chrome on Android) | "Notifications aren't supported in this browser", disabled | – |
| Blocked | `Notification.permission === "denied"` | "Notifications blocked in site settings", disabled | – |
| Off | otherwise, setting not on | "Turn on notifications" | asks for permission if needed, then turns on |
| On | setting on and permission granted | "Notifications on" (`aria-pressed="true"`) | turns off |

- **The permission prompt appears only from that button,** never on load or on an event.
  - The click handler calls `document::eval("return await Notification.requestPermission()")` directly, not in `spawn`; only the wait is. dioxus-web 0.7.9 runs `onclick` synchronously and runs an eval's script when it's made (`clipboard.rs` relies on the same), so the request is inside the gesture.
  - Once granted, the setting is stored and one confirmation notification is shown ("Notifications are on"). If the constructor throws, the mode is Unsupported and the setting isn't stored.
- **Other tabs** read the mode on load, on `storage` events for that key, and on `focus`. Turning it on makes every tab request the lock; turning it off makes the leader release it and drop its stream, and the leader re-reads it before each notification (step 1).
- **Where it sits:** in the sidebar's links (272 px; the longest label, ~45 characters at 13 px, wraps to two lines), above the conversation list, which moves down a row or two; it covers nothing. Below 720 px it's behind the Conversations button (SME-41 D4). Settings pages have no sidebar, so no button, but a tab there still takes part.

## When notifications can't work: the title count

- **Every chat tab, secure or not,** keeps a set of conversations that need the user, filled from the relayed `ConversationEvent::NeedsUser` while the tab isn't visible-and-focused.
  - Its title becomes `(N) smelt`, and back to `smelt` when the tab is focused and visible (`focus`, `visibilitychange`, through the same eval channel).
  - On each `QuestionsChanged`/`TurnsChanged` it revalidates its entries with `get_attention`, so a question answered in another tab drops out of the count.
  - A leader feeds its own count from its app stream.
- **Handing events to the notifier:** the `Shell` layout provides a `NotifierFeed` context (two signals: latest `NeedsUser`, revalidate counter). `live.rs`'s `use_conversation_events` gets a `NeedsUser(…)` `EventEffect` arm writing the first; its `TurnsChanged`/`QuestionsChanged` arms bump the second; `state.rs`'s `apply_event` maps the event to the new effect.
- **The favicon** (decided: keep). smelt has none (its `/favicon.ico` request 404s). This adds `assets/favicon.svg` (an "s" in the accent colour) and `assets/favicon-dot.svg`, swapped with the title; a pinned tab shows only its favicon. The tier harness serves stylesheets only, so a scenario asserts the swap's `href`, never that the svg loads.
- **No sound** (decided): autoplay rules block it until a click, and a sound at every finished turn annoys.
- **Settings tabs and Home over plain HTTP** have no stream, so no count. Deliberate: one stream per tab would use up HTTP/1.1's 6 connections.

## Getting real notifications from another machine

Browsers only allow them on a secure origin, which `http://ryzen.lan:8180` will never be. Two routes, both under `docs/setup.md`'s existing "Dev over HTTPS" (the compose `caddy` service already serves `https://localhost:8443`, a secure context, unchanged here):

1. **This machine, no change:** open `https://localhost:8443` — real notifications work there as shipped.
2. **Another machine, per browser, no server change:** Chrome's `chrome://flags/#unsafely-treat-insecure-origin-as-secure` with `http://ryzen.lan:8180`. This ticket's only setup.md edit is a short paragraph at "Dev over HTTPS" noting that notifications need a secure context, `https://localhost:8443` works today, and this flag is the quick fix elsewhere. Production (`*.constantlee.us`) is HTTPS already.

## State model

### 1. A conversation's attention (server, in memory)

The record is `ConversationRuntime.attention`; the generation is `kept.generation`, read when it's published.

| State \ event | `ask` | cut-off / refusal notice saved | run ends Failed (`Failed` or `Noted`) | run ends Ok with an assistant message | run ends Ok, none | Stop | last turn ends | a turn starts (from idle) | user sends or answers | conversation deleted | restart |
| -- | -- | -- | -- | -- | -- | -- | -- | -- | -- | -- | -- |
| idle, none | – | – | – | – | – | – | – | → busy, none | generation +1 | – | – |
| busy, none | → Question | → CutOff / Refused | → Failed | → Finished | stays | stays none | → idle, nothing published | (already busy) | generation +1 | cleared | lost |
| busy, X | max(X, Question) | max(X, …) | max(X, Failed) | max(X, Finished) | stays X | → none | → idle; publish `NeedsUser(X, generation)` | – | generation +1 (the event will be stale) | cleared (a later note can set it again; see below) | lost |

**What each caller leaves when its last turn in flight ends:**

| Caller \ ends | Stopped while queued | Conversation not found | Failed before saving its input | Failed after | Nothing to do | Done |
| -- | -- | -- | -- | -- | -- | -- |
| user's message (`start_turn`) | cleared (SME-170) | Failed noted; `get_attention` → `None` | Failed | Failed | – | Finished, or Question if it asked |
| answer (`answer_question`) | cleared (answer left for the next turn) | as above | Failed | Failed | unchanged | Finished / Question |
| notice (`deliver_notice`) | cleared | as above | Failed | Failed | – | Finished / Question; nothing if a question waits (no reply) |
| wake (`wake_conversation`) | cleared | as above | – (not counted, SME-218) | Failed | unchanged | Finished / Question |

A Stop also clears an earlier Failed or Question from the same busy spell: the user is there.

**Writers:** `ask` (Question); `save_stop_notice` (CutOff, Refused); `run_turn_bounded`'s ending (Failed — `Failed` and `Noted` alike, Finished, the clear on Stop), for all four callers; `admit` (clear); `TurnInFlight::drop` (take and publish); `forget_conversation_lock` (clear) — all under the runtime mutex.

**Readers:** `TurnInFlight::drop` (takes it); `get_attention` reads `turn_running`, `turn_generation`, the question row and the title, never the field.

**Failure part-way:**

- `ask`'s row insert fails: nothing noted; the turn fails, so Failed.
- The cut-off notice's save fails: nothing noted yet (the note follows the save); the turn errors, so Failed.
- Publishing can't fail (a `broadcast` send with no receivers is fine).

**A delete racing a turn.** `forget_conversation_lock` clears the attention, but the running turn notes Failed ("conversation not found", or a save failing) and its end publishes `NeedsUser` for a deleted conversation. `get_attention` finds no title and answers `None`: nothing shows, and chat tabs drop it on revalidation. A guard would need the lock handle in `run_turn_bounded`; not worth one wasted request.

**Between the publish and the check:** a new turn can start after `drop`'s critical section and before `NeedsUser` is sent; the leader hears `TurnsChanged`, then `NeedsUser`, and `get_attention` sees `turn_running` → `None`.

### 2. The leader lock (each tab, in the browser)

| State \ event | setting turned on | focus | lock stolen | page closes or reloads | setting turned off | stream drops |
| -- | -- | -- | -- | -- | -- | -- |
| off | → queued (plain request) | – | – | – | – | – |
| queued | – | steal → leader | – | request aborted (§2.6) | abort the request (an `AbortSignal`) → off | – |
| leader | – | – | `AbortError` → drop the stream, queue again | lock released → the next queued tab leads | release → off | reconnect after 1.5 s, like `use_pods_changed` |

### 3. A shown notification (in the tab that made it)

- **States:** shown, then closed.
- **It closes when:** the user clicks it (the conversation opens in that tab); a revalidation finds it stale (answered elsewhere, a new turn, the conversation deleted); that tab shows the conversation with focus; a new one with the same tag replaces it; the user dismisses it; the page goes away (they live with their page).
- **A demoted leader keeps the notifications it showed.** A chat tab still hears the relayed `QuestionsChanged`/`TurnsChanged` and revalidates; a settings or Home tab doesn't, so a stale one stays until clicked (which just opens the conversation) or dismissed.

**Between the check and the act:**

- *Between `get_attention` and `new Notification`* another tab can answer or a turn start; their `QuestionsChanged`/`TurnsChanged` makes the leader revalidate and close it: shown at most one round trip.
- *A question answered before the event reaches the leader:* `get_attention` → `None`, nothing shows.
- *The user switches smelt tabs while the leader decides:* the new tab steals the lock and an event in flight on the old stream is lost — but the user is in smelt then; likewise during a 1.5 s reconnect or a hand-over.
- *The setting turned off between the event and the show:* step 1 covers it, and turning it off releases the lock.

**A tab reloaded:** its lock passes on; nothing is re-announced (deliberately no catch-up: a waiting question shows as the sidebar's `?` and the card). Its notifications close with the page.

**Who sees each change:** the event reaches every tab with a stream (the leader by `subscribe_app_events`, chat tabs relayed). Only the leader notifies; each chat tab keeps its own title count.

**Work already in flight:**

- The setting is read right before each notification; already-shown ones stay until a close path above.
- A turn waiting for a provider slot (SME-343, merged) or compacting after an overflow refusal (SME-73, merged) is still in flight: nothing notifies mid-recovery, only the turn's end decides the kind, and a Stop while it waits clears the attention as any Stop does.

**Per mode, per tab (the notifier):**

| Tab \ mode | Unavailable | Unsupported | Blocked | Off | On |
| -- | -- | -- | -- | -- | -- |
| chat tab, not leader | count | count | count | count | count; queued; steals on focus |
| any page, leader | – | – | – | – | app stream; notifies; title count |
| Home or settings page, not leader | nothing | nothing | nothing | nothing | queued; steals on focus |
| the sidebar button | disabled, says HTTPS | disabled | disabled | asks, then on | turns off |

## Files

Sizes are now, against the 40,000-byte ceiling (30,000 aim).

**Commit 1, move only,** checked by `scripts/check.sh` (whose test-build warnings check, SME-361, catches a re-export nothing in tests uses):

1. `src/turn/run.rs` (38,109): the run functions move to a new `src/turn/lifecycle.rs`, declared in `src/turn/mod.rs` with `mod lifecycle;` and `pub(crate) use self::lifecycle::*;` beside `run`'s glob: `run_turn` (its doc included, from 27) through `end_stopped_turn` (27–360: `run_turn`, `record_saved`, `run_turn_bounded`, `run_turn_stoppable`, `relay_reply_delta`, `end_stopped_turn`), `record_stop`, `start_turn` (375–443) and `note_drained_activity` (507–519; its callers sit on both sides) — about 20,200 bytes, `run.rs` going to ~17,900. `note_failure` stays (reached by moved code once `pub(super)`), as do `TurnSteps`, `run_turn_body` and `run_turn_steps` — the turn body's own.

No other move is needed: `src/events/mod.rs` is 12,309 and gains ~1,500, its tests already out upstream (Revision 3). Every other changed file is at least ~12,000 clear of the ceiling after its additions.

**New:**

- `src/api/notifications.rs`: `get_attention`, `AttentionDetails`, tests.
- `src/turn/tests/attention.rs`: the server tests (one `mod attention;` line in `src/turn/tests/mod.rs`, 15,904; it follows that module's conventions — `lock_turn_tests`, ids clear of other runs).
- `src/frontend/notifications.rs`: the `Shell` layout component; the `Notifier` (eval channel, lock, the leader's stream, the title count); `NotifierFeed`; the `NotificationsToggle` button; the JS as `const` strings; pure functions for unit tests: `notify_mode`, `should_notify` (setting, focus, route, cooldown), `notification_text`, `badge_title`. Aim under 30,000; the unit tests go in `src/frontend/notifications/tests.rs` from the start.
- `src/browser_tests/notifications.rs`: the scenario (plus its `mod` line and one `entry` line in `browser_tests/mod.rs`, 22,359 — `Runs::Alone`, see Tests).
- `assets/favicon.svg` and `assets/favicon-dot.svg`.
- `docs/frontend/notifications.md` (the notifier, the layout route, the button, the title count), linked from `docs/frontend.md` (34,870; one line).

**Changed:**

- `src/events/mod.rs` (12,309): `Attention`, both `NeedsUser` variants; wire tests in the existing `src/events/wire_tests.rs` (7,619).
- `src/api/mod.rs` (228): one `pub mod notifications;` line.
- `src/api/chat.rs` (25,873): the relay arm in `conversation_relay`, and its test (`src/api/chat/tests.rs` already drives it with a channel of the test's own).
- `src/turn/state.rs` (27,754): the attention field, `note_attention`, `clear_attention`, `turn_generation`, the `admit` clear, the `Drop` take, `is_idle`, `forget_conversation_lock`. Lands ~30,300 — above the aim, far under the ceiling.
- `src/turn/lifecycle.rs` (~20,200 after the move): the notes at `run_turn_bounded`'s end (Failed, the clear on Stop, Finished).
- `src/turn/reply.rs` (16,876): the notes in `save_stop_notice` (CutOff, Refused).
- `src/turn/questions.rs` (7,925): the note in `ask`.
- `src/db/conversations.rs` (14,414): `get_conversation_title`; its test in `src/db/tests/conversations.rs` (17,590).
- `src/frontend/mod.rs` (6,382): `#[layout(Shell)]` around every route and `#[end_layout]`, so `Shell` (rendering `Notifier {}` and `Outlet::<Route> {}`) is inside the router on every page; `mod notifications;`; the favicon `document::Link`.
- `src/frontend/pages/chat/state.rs` (27,416): `EventEffect::NeedsUser` and its `apply_event` arm; a test in `chat/tests/events.rs` (16,286).
- `src/frontend/pages/chat/live.rs` (18,415): the `NeedsUser` arm, and the `NotifierFeed` bump in the `TurnsChanged` and `QuestionsChanged` arms.
- `src/frontend/pages/chat/sidebar/mod.rs` (23,907): `NotificationsToggle {}` after the Language servers link (line 218).
- `src/frontend/pages/pods.rs` (25,195): nothing; `use_pods_changed`'s `_ => {}` already skips the new `AppEvent` (re-confirmed after SME-304). The Developer confirms.
- `assets/css/07-pages.css` (22,113): `.notifications-toggle`, light and dark, next to `.settings-link`.
- Docs: `docs/setup.md` (31,527; one paragraph at "Dev over HTTPS": notifications need a secure context, `https://localhost:8443` works today, the Chrome-flag route — no Caddyfile or compose change), `docs/api/events.md` (31,479; `NeedsUser`, `Attention`), `docs/api.md` (29,933; `get_attention` under "Current endpoints"), `docs/architecture/modules-core.md` (20,233; the three new modules), `docs/testing/browser-tier.md` (21,636; the scenario, the Notification stub).
- At close-out: `docs/current-state/features.md` (30,493; one bullet), `docs/current-state/architecture.md` (21,657; the events line), and a Feature checklist row in `docs/feature-checklist/conversations.md` (patch edit, read back).

**Not touched:** `pages/chat/mod.rs`, `transcript.rs`, `clipboard.rs`, `turn/notify.rs` (SME-79's; an idle-notice follow-up would change it), `turn/overflow.rs`, `src/sandbox/*`, the agent, `Cargo.toml`/`Cargo.lock` (no new crate; browser APIs go through `document::eval`).

Tests reaching `TurnInFlight`/`run_turn_bounded` (every `src/turn/tests/*`, `browser_tests/{basics,notices}.rs`) need no new row or server: the attention is in memory.

### Overlap

Against Planned, Todo, In Progress, In Review and Approved (Linear, 2026-11-03: only SME-229) and every open PR (only #236):

- **SME-229 (In Progress, PR #236):** `scripts/cluster-doctor`, `src/bin/sandbox_image_import.rs`, new `src/bin/sandbox_image_import/disk.rs`, `docs/process/rules-commits.md`, `docs/setup.md` (its disk-pressure paragraph), `docs/testing/sandbox.md`. No shared code file; the `docs/setup.md` hunks are sections apart, and #236 merges first. Not a blocker.
- **SME-79** (Done, idle notices): changes `turn/notify.rs`, untouched here. Not a blocker; its idle notices wake a forgotten command's conversation every 5 minutes, and each such turn notifies Finished (Trade-offs).
- The old blockers are history: **SME-185** and **SME-73** are Done, and this plan is revised against both merges. Nothing else sits in Planned, Todo, In Progress, In Review or Approved today.

## Tests (each seen failing first)

Red predictions are marked "expected"; the Developer records the actual red next to them.

### Server unit and mock-upstream tests

In `turn/tests/attention.rs`, `api/notifications.rs`, `events/wire_tests.rs`, `api/chat/tests.rs`. Ids and locks follow `src/turn/tests/mod.rs` (docs/testing.md "Tests that touch per-conversation state", `docs/testing/techniques.md`); the shared app channel means each test counts only its own conversation's `NeedsUser` (helper `events::AppRecording`).

- One `NeedsUser` when the conversation goes idle, for each outcome:
  - `ask_user` → Question;
  - a cut-off reply (mock `stop_reason: max_tokens`) → CutOff; a refusal (`stop_reason: refusal`) → Refused;
  - an upstream 500 → Failed; a notice's turn (`deliver_notice`) failing → Failed; a wake failing → Failed; `MAX_TURNS` → Failed;
  - a user turn whose call fails so its failure notice is saved (`TurnFailure::Noted`) → Failed, once;
  - a plain reply → Finished, for each of `start_turn`, `answer_question`, `deliver_notice` and a wake with a pending command notice.
- **SME-73 pair:** a turn that overflows once, compacts and answers → one Finished, no Failed; a turn refused a second time (mock refuses twice with a named window) → Failed.
- Nothing published for: a Stop mid-turn; a Stop catching a queued message (SME-170 path); a no-op wake; a notice saved while a question waits; a turn ending on `ask_parent` (the wait-aware Ok rule notes nothing; amended 2026-11-03).
- A queued notice turn that finishes after a failed run publishes Failed (the maximum), once.
- A Stop after a Question in the same busy spell publishes nothing.
- `NeedsUser` comes after `TurnsChanged` on the app channel and after `ConversationEvent::TurnState { running: false }` on the conversation's, and carries `turn_generation` as at that end.
- The runtime entry is still freed when nothing else holds it (`has_turn_state` false after a delete and a published outcome).
- `turn_generation`: 0 with no entry; follows `new_turn_generation` and `take_turn_generation`.
- `get_attention`:
  - `Some` for a fresh event, with the title and, for a question, its first header;
  - `None` for a stale generation (`start_turn` since);
  - `None` while a turn runs;
  - `None` for a question answered, for one dismissed by the user's message, and for a parent-row question (asked_of parent);
  - `None` for a deleted conversation (including the delete-racing-a-turn case: delete while the mock upstream holds the turn, then release it).
- The wire round trip of `Attention` and both variants; an unknown `type` still decodes as `Unknown`.
- The relay arm in `conversation_relay` (a channel of the test's own, as the existing `src/api/chat/tests.rs` tests do).

Which part each test needs: the outcome tests need the notes *and* `drop`'s publish (with only the notes, nothing is published: expected red "no NeedsUser within 5 s"); the "Failed wins" test needs `note_attention`'s maximum (with a plain overwrite it sees Finished); the `Noted` test needs the note to cover `TurnFailure::Noted` (with only `Failed` matched it sees nothing); the "Stop after Question" test needs the clear on `Err(Stopped)`; the ordering test needs the publish after `TurnsChanged` (publishing inside the critical section, before it, fails it). The Developer stubs each part once for its red (tdd.md, "Check that a fixture set bites").

### Frontend unit tests

In `src/frontend/notifications/tests.rs`, plain functions: `notify_mode` for every row of the mode table; `should_notify` (setting off, focused on that conversation, focused on another, hidden, the cooldown for Finished only); `notification_text` for the five kinds; `badge_title` for 0, 1 and N. In `chat/tests/events.rs`: `apply_event` maps `NeedsUser` to `EventEffect::NeedsUser`.

### Browser tier (new scenario `desktop_notifications`)

It runs on `127.0.0.1` with chrome-headless-shell, a secure context. `Runs::Alone`, reason in the entry comment: the leader holds the app-wide stream, so other lanes' turn ends would land in this scenario's counts; and it's the only scenario that touches a browser-wide setting.

- **Spike first:** that headless shell has `navigator.locks` and `focus`/`steal` behave as needed; if `focus` doesn't fire between headless tabs, the scenario dispatches it to force leadership and leaves the real focus path to the hands-on check.
- **The stub** (`evaluate_on_new_document`, each tab): `window.Notification` with `permission: "granted"`, `requestPermission` → `"granted"`, each construction (heading, body, tag, renotify) recorded in `window.__notifications` with `close()` and a callable `onclick`.

1. Tab A on conversation C1, tab B on `/providers`. Click the button in A: it reads "Notifications on", and `localStorage` holds the setting.
2. A mock upstream (as `ask_user_card`'s) makes C2's turn call `ask_user`. Across both tabs exactly **one** notification is recorded, with tag `smelt-conversation-C2` and C2's title in the body.
3. Calling its `onclick` moves that tab to `/conversation/C2`.
4. Another C2 question; answer it on the card in the other tab: the recorded notification's `close()` was called.
5. Reload whichever tab leads. A finished turn in C1 still gives exactly one notification, from one tab, and the reload didn't re-announce C2.
6. Turn the setting off: the next finished turn records nothing.
7. **Plain HTTP:** a tab at `http://smelt-http.test:<port>/conversation/C1` (the tier's existing `INSECURE_HOST` mapping, harness.rs), where `isSecureContext` is false.
   - The button reads "Desktop notifications need HTTPS" and is disabled.
   - No `Notification` is constructed.
   - After a finished turn in C1 (started from a `127.0.0.1` tab, since writes from the plain-HTTP name can be refused, SME-146) while this tab is unfocused, `document.title` is `(1) smelt`, and back to `smelt` once it has focus (forced by dispatching `focus` and `visibilitychange` if headless won't move focus).
8. At 390 px the button sits inside the opened sidebar list, with no sideways scroll; at 721 px, with the widest label, it wraps inside the 272 px sidebar without overflowing.

Each check is run once with its piece of the feature switched off, and must fail.

### Hands-on check

From the `docs/personas/rules.md` template, as `<scratchpad>/sme-71/hands-on.sh`: `CHECK_SCRATCH_DB=1`, `CHECK_WORKTREE` a check worktree of this branch (SME-115's fix), started with `-- SMELT_ALLOWED_HOSTS=smelt-http.test` (replaces the dev `.env`'s, so the plain-HTTP tab's reads pass). No cluster lock (SME-100).

- A mock Anthropic-compatible provider added in the UI calls `ask_user` first, then answers plainly (as SME-34's hands-on). Never the user's llama-server.
- A Playwright script on `scripts/ui-check/smelt_ui.py`'s `run`, in Chromium with `--host-resolver-rules=MAP smelt-http.test 127.0.0.1`, checks with the stub: leader election across two real tabs with real focus changes (`page.bring_to_front()`), the click opening the conversation, the plain-HTTP button and title count (messages sent from the `localhost` tab, SME-146), and a leader left backgrounded for minutes (tab freezing). Sidebar screenshots at 1400 and 390 px, light and dark. `run` deletes its conversations before `stop`.

### After the merge (for the user, in What shipped)

- In a real Chrome on `https://localhost:8443`, click "Turn on notifications". Switch to another app, send a long task, check that one desktop notification arrives and that clicking it opens the conversation.
