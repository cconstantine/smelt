# Wake the model when a terminal command actually finishes

**Branch:** `terminal-exit-notify` · **Plan:** `projects/plans/terminal-exit-notify.md` (removed)

Not from an idea file — found by inspecting a real, live conversation
(#60 in the local dev database) where the model repeatedly said it would
wait for a terminal command's completion notification and never got one.

## What shipped

**Root cause.** `run_terminal_command`'s tool description promises "you'll
also be notified here when it finishes, with no further tool call
needed" — that was false. A terminal command's `"exit"` event
(`sandbox.rs::handle_agent_message`) only marked it finished in the
database and published a browser-facing SSE event; nothing ever called
back into `chat::run_turn`. The only thing that actually notified the
model was `db::unnotified_finished_terminal_commands`, drained at the top
of `run_turn_bounded`'s own loop — which only runs when something *else*
triggers a fresh turn (a new user message, or — for `run_async` background
tasks specifically — `push_terminal_notification`, which *does* explicitly
call `chat::run_turn` on completion). Terminal commands never got that
same active push. Confirmed live: one command in conversation 60 was
notified 50 seconds late, only because the user happened to send an
unrelated message in the meantime; another was still sitting unnotified
in the database at the time of inspection, with the model's last message
literally "I'll wait for the completion notification."

**The fix.** A new `chat::wake_conversation(pool, conversation_id)`,
called (detached, via `tokio::spawn`) whenever a terminal command reaches
a terminal state — both a normal exit (`handle_agent_message`) and a
crash-induced `'lost'` marking (`handle_crash_cleanup`, the same
underlying gap, not separately reported but equally broken). Rather than
constructing its own notification message, it reuses
`run_turn_bounded`'s own backlog-drain logic (pulled out into a shared
`drain_unnotified_terminal_commands` helper) — if nothing is actually
pending by the time it acquires the conversation lock (e.g. another
concurrent wake, or an unrelated live message, already handled it), it's
a true no-op: no persisted message, no API call. This is what keeps
several commands finishing close together from costing one model turn
each.

Both call sites spawn detached rather than awaiting inline, for two
different reasons: `handle_agent_message` runs inside the per-pod
WebSocket reader loop, where blocking on a full model round trip would
stall processing of any further output/exit events; `handle_crash_cleanup`
can run synchronously from *inside* an already-in-progress
`run_turn`/`execute()` call that's already holding the very lock
`wake_conversation` needs — awaiting it directly there would deadlock,
the same hazard `cancel_task_tool`'s own comment already documents for
the analogous `run_async`-cancellation case.

**Silent-failure visibility.** A detached `wake_conversation` call that
fails (missing `ANTHROPIC_API_KEY`, a transient Anthropic API error) now
publishes `ConversationEvent::NotificationDeliveryFailed { detail }` on
the existing persistent per-conversation event bus, alongside a
`tracing::warn!` for server-side debugging — rendered in the browser the
same way a failed `send_message` call already renders `stream_error`. The
underlying notification text is unaffected either way: it's durably
persisted by the drain step, which runs and commits *before* the API call
that might fail, so this event means "the model hasn't seen it yet," not
"it's lost."

**Verification:** 142 `cargo test --features server` tests (four new
mock-upstream tests covering `wake_conversation`'s no-op, drain, coalescing,
and failure-publishing behavior; the real-cluster integration test
extended to prove a bare command exit — no other trigger — produces a
notification), WASM check clean, and the automated browser test.

## Retrospective

**What worked:**
- Diagnosing against the real dev database directly (a throwaway
  standalone `sqlx`/`tokio` scratch binary, not guesswork) turned a vague
  "sometimes doesn't wake up" report into two concrete, provable
  reproductions — one command notified 50 seconds late, one still sitting
  unnotified at inspection time — before a single line of code changed.
- Reusing `run_turn_bounded`'s own backlog-drain logic for
  `wake_conversation`, instead of building a parallel notification path,
  meant the coalescing behavior (a second near-simultaneous wake finding
  nothing left to do) fell out for free, and every existing test covering
  the drain logic kept passing unchanged straight through the refactor.
- Resolving the "silent-failure visibility" open question as part of the
  plan, before implementation started, meant the new `ConversationEvent`
  variant and its frontend rendering were designed in from the start
  rather than bolted on after the fact.

**What caused friction, surprise, or rework:**
- **The real-cluster integration test hung indefinitely the first time it
  ran**, because `ANTHROPIC_API_KEY` turned out to be genuinely present in
  this environment's real process env (not just the app's own `.env`) —
  every terminal command completing throughout that whole test now fires
  a real `wake_conversation` call, which made a genuine, uncontrolled
  request to the live Anthropic API, and one of them hung waiting on
  network egress this sandboxed environment doesn't have.
  `sandbox.rs`'s real-cluster tests had never needed to think about this
  before, since nothing in that file previously touched the Anthropic API
  at all — this change made a previously-inert code path start firing
  into a different subsystem, and the test needed the same isolation that
  subsystem's own tests already use (`anthropic::test_support::lock_anthropic_base_url`
  plus redirecting `ANTHROPIC_BASE_URL` to an unreachable local address),
  even though the test's own code never otherwise mentions Anthropic.

**What to change (proposal):**
- When a change makes a previously-inert code path start firing calls
  into a different subsystem — especially indirectly, via a detached
  background task rather than a call the test itself makes — check
  whether tests exercising that path now need the same test isolation
  that subsystem's own tests already rely on. It's easy to miss precisely
  because the affected test's own code never mentions the subsystem at
  all.
