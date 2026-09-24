# Bug bash

**Branch:** `bug-bash`

## What

A deliberate hunt for bugs across the app, done 2026-09-24: two code sweeps (the chat turn loop and streaming, the chat UI's state, background tasks, the event bus, sandbox volumes) and three hands-on sweeps against a real dev server with a real model (core chat flows, reloads, a second tab, the settings pages). This plan lists what was found and proposes fixing it.

Every finding is marked:

- **Confirmed** — reproduced in the running app or a real test, or proven directly (e.g. a serialization call that errors).
- **Confirmed by code** — the defect is plain from the code path, named below, but hasn't been triggered live (usually because it needs conditions this environment doesn't have).
- **Suspected** — plausible, not shown.

## Findings

### High

**1. New messages never reach an open page except the tab that sent them.** *Confirmed.*
`ConversationEvent` is serialized with an internal tag (`#[serde(tag = "type")]`), and `MessagesAppended(Vec<Message>)` is a tuple variant holding a list, which serde can't serialize that way: every attempt returns "cannot serialize tagged newtype variant ConversationEvent::MessagesAppended containing a sequence". `subscribe_conversation_events` discards the send error (`let _ = tx.send(event)`), so the event silently never leaves the server.
- A second tab on the same conversation saw neither the user's message nor the reply until it was reloaded.
- Reloading while a reply streams: the reply was saved 10 seconds later, and the page still hadn't shown it 15 seconds after that; it appeared only on a second reload.
- A fresh subscriber (`curl` on the event stream) received the `ContextUsageUpdate` published by the same turn, but no `MessagesAppended`.
- By the same mechanism (not triggered live here), every reply the model produces without a live send — after a background task finishes, after a terminal command finishes — never appears until a reload.
- The existing tests pass because they check the event inside the server, before serialization.

Fix: make it a struct variant (`MessagesAppended { messages: Vec<Message> }`), and stop discarding send errors in the subscription loop (log them). Tests: a serialization round trip for every `ConversationEvent` variant, and a browser-tier scenario where a second tab sees a new message live.

**2. A volume whose claim is missing stops every new sandbox, reported only as "Timeout".** *Confirmed — and first misdiagnosed.*
Every configured volume is mounted into every sandbox pod. The bug bash created a volume (with the relative mount path `relative/path`) from the dev server, so its Kubernetes claim was created in the dev namespace; the browser tier, which shares the dev database but creates pods in the test namespace, then failed every pod creation with `create_pod: Timeout`, and deleting the volume made it pass. This was first blamed on the relative path. A real-cluster probe disproved that — a pod with a relative mount path started in 5 seconds — and a second probe showed the real cause: the claim didn't exist in that namespace, so the pod sat `Pending` (`PodScheduled=False`, "persistentvolumeclaim "sandbox-volume-…" not found") until the timeout, which reported none of that.
- The same happens outside tests whenever a claim is missing: after a cluster rebuild (which this environment went through the same day), or after a claim is deleted.

Fix: create any missing volume claim before creating a pod; include the pod's own explanation in the timeout error; fail fast (with the reason) when a container is stuck in a state that doesn't recover (image pull failures, crash loops). Separately, validate that a mount path is absolute, as the form's own help text requires — not a breakage fix, since relative paths turned out to work. Tests: a real-cluster test that a pod mounting a volume with no claim starts (it timed out before), plus unit tests for the timeout detail, the fail-fast check and the path validation.

**3. Compaction re-summarizes the whole history every time, until it can't.** *Confirmed by code.*
`compact_conversation` builds its summarization transcript from *every* message in the conversation (`src/api/chat.rs`, the loop over `messages`), including everything an earlier compaction already replaced. Each later compaction's input is therefore larger than the last one's, while the context window stays fixed. Once the full history no longer fits, the summarization call fails; a failed compaction fails the turn; and since the conversation is still over the threshold, every later turn tries again and fails. The conversation is then unusable. Not reproduced: it needs a conversation long enough to compact at least twice.

Fix: build the transcript from the latest compaction summary plus the messages after its boundary (the same cut `history_for_request` already makes), and cap the transcript to fit. Test: a unit test that a second compaction's transcript excludes messages before the first boundary.

### Medium

**4. An unknown conversation looks usable and shows a raw database error.** *Confirmed.*
`/conversation/99999999` renders a normal, enabled chat box. Sending shows "error returned from database: insert or update on table "messages" violates foreign key constraint…". Fix: treat a missing conversation as "not found" (a clear message, no input), and make `send_message` return a plain "conversation not found".

**5. Model-provider errors are shown raw, with no retry.** *Confirmed.*
When the configured provider returned HTTP 503 (temporarily paused), the page showed the whole response — nested JSON and a trailing "(details: None)". A transient 503/529 isn't retried. Fix: a short bounded retry for 429/503/529, and a readable message (the provider's message, not the JSON wrapper) when it still fails.

**6. The sidebar never refreshes a conversation's title.** *Confirmed.*
The sidebar loads its list once per page (`use_resource(get_conversations)`); a new conversation keeps showing "New Conversation" after its first message until a reload. Fix: publish a title change (or refetch the list) when the first user message sets it.

**7. Every live event subscription leaks when the tab goes away.** *Confirmed by code.*
`subscribe_conversation_events` uses `ServerEvents::new`, whose loop only ends when the conversation's channel closes — which it never does — and ignores send errors, so it never notices a disconnect. Every page load, reload or reconnect leaves a task and a channel receiver behind for the server's lifetime. This is the same pattern already fixed for the browsing panel's frame stream in #22. Fix: the same fix — `ServerEvents::from_stream`, so a closed connection drops the subscription.

**8. A failed background-task notification disappears silently.** *Confirmed by code.*
When a `run_async` task finishes, `push_terminal_notification` (and `record_task_line` for streamed output) runs a model turn and discards its result (`let _ = chat::run_turn(...)`). The terminal-command path (`wake_conversation`) publishes `NotificationDeliveryFailed` on the same failure; this path doesn't. Fix: route both through the same failure reporting.

**9. Deleting a conversation leaves its background tasks running.** *Confirmed by code.*
`delete_conversation` tears down sandboxes and the browsing session but doesn't cancel the conversation's `run_async` tasks. When one finishes it tries a turn for the deleted conversation, which fails on the foreign key (silently — see #8). Fix: cancel and remove the conversation's tasks on delete.

### Low

**10. A background-notification error follows you to every conversation.** *Confirmed by code.* `notification_delivery_error` in `chat.rs` is set when the event arrives and never cleared — not on a conversation switch, not after a later success. Fix: reset it on switch, like the other per-conversation state.

**11. The context-usage indicator shows the previous conversation's numbers after a switch.** *Confirmed by code.* `context_usage` isn't reset on a switch, only replaced when the new conversation's snapshot arrives. Fix: reset it with the rest.

**12. In-memory registries only grow.** *Confirmed by code.* The task registry (`TASKS`, including every task's full stdout/stderr), the event channels (`BUSES`) and the per-conversation locks (`CONVERSATION_LOCKS`) are never pruned, even for deleted conversations. Small per entry, unbounded over a server's lifetime. Fix: drop a conversation's entries on delete; drop finished tasks after a while.

**13. With no model credentials configured, a sent message vanishes on reload.** *Confirmed by code.* `run_turn_bounded` checks credentials before saving the user's message, so the message shows on screen (optimistically), the error shows, and a reload loses the message. Fix: save the message first, then fail.

**14. A multi-step reply streams as one merged bubble.** *Suspected (UX).* `send_message` only sends `Done` events after the whole tool-use loop finishes, so text from several rounds accumulates in one streaming bubble and the intermediate tool calls appear only at the end. Worth a look alongside #1.

**15. `dx`'s "Your app is being rebuilt" overlay shows with no rebuild happening.** *Suspected, dev-only.* Seen on a dev server started with `--watch false`, and on the browser tier's in-process server. It doesn't block use (text is still readable) but it's misleading. Worth understanding before relying on it as a signal.

### Checked and fine

Empty and whitespace-only messages are ignored; a double Enter sends once; HTML-like text in messages renders as text; a ~36,000-character message works; deleting a conversation (two-click confirm) works and returns home; the MCP-server form blocks empty required fields with the browser's own validation.

## How

Fix in severity order, one commit per finding, each starting with a test that fails on the current code (per development-process.md's rules, including seeing every check fail first). #1 first: it's the root of several symptoms, and #7 touches the same function, so they go together. #2 and #3 next. #4–#9 after, then the low ones. Given the size, probably two or three PRs: #1+#7, #2+#3, then the rest.

## Which files

- `src/events.rs` — #1 (variant shape), plus a round-trip test.
- `src/api/chat.rs` — #1/#7 (subscription loop), #3 (compaction transcript), #4 (not-found), #5 (retry/message), #9 (delete), #13 (save before credential check).
- `src/anthropic/stream.rs` — #5 (retry on transient statuses).
- `src/anthropic/tools.rs` — #8, #9, #12 (task registry).
- `src/sandbox.rs` — #2 (mount-path validation, `create_pod` error surfacing).
- `src/frontend/pages/chat.rs` — #4 (not-found view), #6 (sidebar titles), #10, #11.
- `src/frontend/pages/sandbox_volumes.rs` — #2 (form validation).
- `src/browser_tests.rs` — second-tab scenario (#1), unknown-conversation scenario (#4).

## Open questions

- **#3:** summarize only since the last boundary (cheap, loses nothing the earlier summary didn't already keep), or also cap the transcript by size? Proposing both.
- **#5:** how many retries, and should a paused provider (503 with a "retry later" message) be retried at all, or shown immediately?
- **#6:** publish a `ConversationEvent` for title changes (live in every tab), or just refetch the sidebar list after a send completes (simpler, only the sending tab)? Proposing the event.
- **#12:** how long to keep a finished background task's record before pruning it?

## Leftovers from the bug bash itself

The hands-on sweeps created about ten conversations in the dev database (all today, most titled "New Conversation"), and one invalid sandbox volume, which has been deleted (that deletion is what proved finding #2).
