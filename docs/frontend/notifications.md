# Desktop notifications (SME-71)

When a conversation needs the user — a question or a repo trust card, a failed turn, a reply cut off or refused, or the model calling `notify_user` to say it's done (SME-377; a turn ending notifies nothing by itself) — smelt says so: a desktop notification from one smelt tab (on any page), and a count in each chat tab's title. The server side (who owes what, and `get_attention`) is in [api/events.md](../api/events.md#app-wide-events-and-pods); this file is the browser's half.

## One leader per browser, by Web Lock

`Notification` and `navigator.locks` are secure-context APIs: over plain HTTP (`http://ryzen.lan:8180`) there are no notifications at all, only the title count.

Every tab with the setting on (`localStorage["smelt.notifications"] = "on"`, per browser and origin like the permission, off by default) calls `navigator.locks.request("smelt-notifier", …)` and holds the lock until its page goes away. The holder is the **leader**, and only the leader notifies — one notification however many tabs are open; the rest queue. On `focus` a tab that isn't the leader requests the lock with `steal: true`: the old leader's request rejects with `AbortError`, it drops its stream and queues again without `steal`. So the leader is the tab the user last focused, and it only ever checks *its own* focus: no focus map, no messages between tabs.

The leader opens one extra stream, `subscribe_app_events`, on any page — one extra connection per browser. Each notification's `tag` is `smelt-conversation-<id>` with `renotify: true`, so if two leaders ever overlap, the second replaces the first. (Two origins open at once — `localhost:8180` and `localhost:8443` — each elect a leader: locks and tags are per origin. A trade-off.)

The notifier lives in the `Shell` layout route, so it stays mounted across navigation and can use `use_navigator`.

## What the leader does with an event

`live.rs` hands relayed `NeedsUser`s to the layout's `NotifierFeed` context (a queue plus a revalidate counter bumped on `TurnsChanged`/`QuestionsChanged`); the leader's own app stream arrives through the queue it owns. A chat tab that also leads therefore hears one event twice — relay and stream — and one event shows one notification: the leader remembers the `(attention, generation)` each open notification was shown for and skips a repeat. For each event:

1. Read the setting (the JS re-reads `localStorage` on every tick, on `storage` events, and again at construct) — off, nothing.
2. If this tab is visible, focused and shows that conversation, nothing: the user is looking at it.
3. Call `api::notifications::get_attention(conversation_id, generation, attention)` — the check right before acting. `None` means the event is stale (a turn running again, the generation moved on, the question answered/dismissed/asked of a parent with no trust card waiting, a `Notified` whose message the server no longer holds, the conversation gone) and nothing shows.
4. Show `new Notification(heading, { body, tag, renotify: true })`: "smelt has a question" / "smelt: the turn failed" / "smelt: the reply was cut off" / "smelt: the model refused", with the conversation's title as body and, for a question, the first question's header after it (`Trust a repo?` for a trust card alone); for the model's `notify_user`, "smelt: <title>" with its message as body. No kind has a cooldown (the 30 s one held back the automatic Finished, which is gone).
5. Keep the `Notification` in a JS map by conversation id: clicking it focuses the window and sends `click` to Rust, which pushes the route and closes it; every `QuestionsChanged`/`TurnsChanged` revalidates the open ones and closes the stale; this tab focused on that conversation closes it too.

All the deciding is pure Rust (`should_notify`, `notification_text`, `notify_mode`, `badge_title` — unit-tested in `notifications/tests.rs`); the JS in `NOTIFIER_JS` is one long-lived `document::eval` per tab that only reports and constructs.

## The button, and the permission prompt

`NotificationsToggle` sits in the sidebar after "Language servers" (`class="settings-link sandbox-volumes-link notifications-toggle"`; `.notifications-toggle` in `assets/css/07-pages.css`). Its label is the mode: **Unavailable** ("Desktop notifications need HTTPS", disabled, with a `title` naming `https://localhost:8443`), **Unsupported** ("Notifications aren't supported in this browser" — the constructor threw, Chrome on Android), **Blocked** ("Notifications blocked in site settings"), **Off** ("Turn on notifications"), **On** ("Notifications on", `aria-pressed`).

The permission prompt appears **only from that button**, never on load or on an event: the click handler calls `document::eval("return await Notification.requestPermission()")` synchronously (dioxus-web runs handlers synchronously and starts an eval's script where it's made — `clipboard.rs` relies on the same), so the request is inside the user gesture; only the wait for the answer is spawned. Once granted, the setting is stored and one confirmation notification is shown. If the constructor throws, the mode is Unsupported and the setting isn't stored. Other tabs read the mode on load, on `storage` events for that key, and on `focus`; turning it on makes every tab request the lock, turning it off makes the leader release it and drop its stream.

## The title count

Every chat tab — secure or not — keeps the set of conversations that need the user from the relayed `ConversationEvent::NeedsUser`, filled only while the tab isn't visible-and-focused (focused and visible, the user is reading it anyway). Its title becomes `(N) smelt`, and back to `smelt` when the tab is looked at; the favicon (`assets/favicon.svg`, an "s" in the accent colour, and `favicon-dot.svg` with the waiting dot, linked from `App`) swaps with it, so a pinned tab reads the state too. `QuestionsChanged`/`TurnsChanged` revalidate each entry with `get_attention`, so a question answered in another tab drops out of the count. Settings tabs and Home over plain HTTP hold no stream, so no count — deliberate: one stream per tab would use up HTTP/1.1's 6 connections.
