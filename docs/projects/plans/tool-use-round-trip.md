# Tool-use round trip

**Branch:** `tool-use-round-trip` · **Idea:** [`projects/ideas/coding-session.md`](../ideas/coding-session.md) (first of several plans against this idea)

## What

The `coding-session` idea has a real dependency chain: `ContentBlock`
needs `tool_use`/`tool_result` variants and `send_message`'s loop needs to
round-trip them before there's anything to attach a real tool to. This plan
is that first, smaller step, and is scoped to it alone — proving the
tool-use round trip end to end (wire types → multi-turn Anthropic loop →
persistence → minimal rendering) using one trivial, in-process,
side-effect-free tool (`add(a, b)`). No sandbox, no shell/filesystem
access, no container lifecycle, no credential handling — none of that is
designed or decided here. Sandboxing is a separate, later plan against the
same idea doc, written once this round trip is proven and once its own
open questions (isolation mechanism, repo scope, lifecycle) are worked
through on their own.

This plan also proves a second *execution shape* for tool calls: a
generic mechanism for running **any** tool call asynchronously, plus a
full suite of tools for managing what it starts — modeled on OS process
management (`ps`/`wait`/`kill`) rather than invented from scratch, since
that's a well-understood shape for "a thing is running somewhere, and I
need to list, inspect, wait on, or stop it." `count(target,
interval_seconds)` itself stays a perfectly ordinary, fully synchronous
tool — it blocks internally, sleeping `interval_seconds` between
increments, and only returns once it's counted all the way to `target`
(a deliberately slow tool, chosen so there's something worth wrapping).
Backgrounding is layered on top by a separate wrapper tool,
`run_async(tool, input)` — like `fork`+`exec` — which the model calls
*instead of* calling `count` directly when it wants the non-blocking
version: it spawns the named tool's real (synchronous) implementation on
a background task and returns a task id immediately, without waiting for
it. A generic, tool-agnostic suite of follow-up tools then treats that id
like a process handle: `list_tasks` (`ps`) enumerates every task running
or finished in the current conversation; `task_status` (a non-blocking
`ps <pid>`) and `task_output` (`tail`) check in without waiting;
`task_result` reads the final value once done; `wait_task` (`waitpid`,
with a mandatory bounded timeout) blocks the *current* tool call until
the task finishes or the timeout elapses — this is what "notified of
results" concretely means here, since Anthropic's protocol has no
push channel into a conversation that isn't currently running: the model
asks to be told, in a call it explicitly chose to make, and waits (up to
a small bound) for the answer, rather than something interrupting it out
of band; and `cancel_task` (`kill`) best-effort aborts a still-running
one. See "How" below for why the protocol shapes it this way and the
mechanics of each tool.

Per existing guidance ([[smelt_purpose_is_coding_agent]]), tools are
attached to *every* `send_message` call unconditionally — no per-conversation
mode toggle, no `mode` field on `Conversation`.

## Why

Same as the idea doc: smelt's purpose is to be a coding agent, and every
conversation is a coding session. This slice exists on its own because the
full idea (wire types + sandbox + streaming visibility + persistence) is
too large for one branch, and sandboxing in particular is a large,
separate design problem — better to prove the Anthropic tool-use protocol
round-trips correctly through this codebase's existing streaming/persistence
machinery with a safe, trivial tool first, and design sandboxing
separately against a proven loop rather than alongside an unproven one.

## Which files

- **`src/anthropic/types.rs`** — add `ContentBlock::ToolUse { id, name,
  input: serde_json::Value }` and `ContentBlock::ToolResult { tool_use_id,
  content: String, is_error: Option<bool> }` (skip `is_error` when
  `None`), matching Anthropic's wire tags. Add `ToolDefinition { name,
  description, input_schema: serde_json::Value }` and a `tools:
  Vec<ToolDefinition>` field on `CreateMessageRequest` (`#[serde(default,
  skip_serializing_if = "Vec::is_empty")]`, additive like `system`).
- **`src/anthropic/tools.rs` (new)** — the tools for this stage, all
  dispatched through one `async fn execute(conversation_id: i64, name:
  &str, input: &serde_json::Value) -> Result<String, String>` — `execute`
  gains a `conversation_id` parameter it didn't need before, purely so
  `list_tasks` (below) can scope its answer to the calling conversation
  rather than leaking every task across every open chat:
  - `add(a, b) -> f64` — unchanged, instant, no side effects.
  - `count(target, interval_seconds)` — deliberately slow and fully
    synchronous: loops, `tokio::time::sleep`ing `interval_seconds`
    between increments (both clamped, e.g. to `1..=5`; exact bounds
    flagged in Open questions), and only returns once it reaches `target`
    (e.g. `"Counted to 5"`). It has **no** awareness of being wrapped, no
    task id, no job map of its own — from `count`'s own point of view
    it's just a slow version of `add`. The *one* concession to being
    wrappable: after each increment, it checks a task-local "am I running
    inside `run_async`?" context (see below) and, only if present,
    appends a line to that task's output log — purely additive, a direct
    synchronous call to `count` behaves exactly as if that check always
    came back empty.
  - `run_async(tool, input) -> String` — the generic wrapper, `fork`+`exec`
    for tool calls. Validates `tool` names a real, non-`run_async`,
    non-`task_*` tool (rejects nesting and unknown names immediately,
    `is_error: true`, no task spawned), then `tokio::spawn`s a task that
    sets a task-local current-task-id context (e.g. `tokio::task_local! {
    static CURRENT_TASK: String }`, entered via
    `CURRENT_TASK.scope(task_id.clone(), execute(conversation_id, tool,
    input)).await`), stores the spawned task's `AbortHandle` (for
    `cancel_task`) and a `tokio::sync::Notify` (for `wait_task`) alongside
    a new registry entry, and calls `execute` inside that scope,
    recording the eventual `Result<String, String>` and firing the
    `Notify` on completion. The task id reuses the `run_async` tool_use's
    own `id` — same "id already exists, don't mint a new one" pattern
    used elsewhere in this plan. Returns immediately with a short string
    naming the task id and pointing at the rest of the suite (e.g.
    `"Started task toolu_01Ab... running count; use list_tasks/
    task_status/task_output/task_result/wait_task/cancel_task with this
    id."`).
  - `list_tasks() -> String` — `ps`. Returns a JSON array (as a string,
    same convention as every other tool result here) of every task
    belonging to `conversation_id`, e.g. `[{"task_id":"toolu_01Ab...",
    "tool":"count","status":"running"}, ...]` — the one tool in this suite
    that needs the conversation scope, since without it the model could
    see (and poke at) another conversation's tasks.
  - `task_status(task_id) -> String` — non-blocking `ps <pid>`:
    `"running"`, `"finished"`, `"failed"`, or `"cancelled"`; `is_error:
    true` for an unknown id.
  - `task_output(task_id) -> String` — `tail`: the accumulated log for
    that task so far in one string (may be empty — only `count`
    currently ever writes to it); `is_error: true` for an unknown id.
  - `task_result(task_id) -> String` — the wrapped tool's final return
    value once `Finished`; `is_error: true` if `Running` ("not finished
    yet — use wait_task or check task_status"), `Failed` (the wrapped
    tool's own error message), `Cancelled` ("task was cancelled"), or
    unknown.
  - `wait_task(task_id, timeout_seconds) -> String` — `waitpid` with a
    mandatory, clamped timeout (e.g. `1..=10`; see Open questions):
    checks status first, and if not already resolved, awaits (via
    `tokio::select!` against the task's `Notify` and a `tokio::time::sleep`
    for `timeout_seconds`) until either the task resolves or the timeout
    elapses, then returns the same shape `task_result`/`task_status`
    would. This is the **one** tool in the whole suite that's allowed to
    hold the current turn (and thus the current `send_message` request)
    open for a nontrivial, bounded amount of time — a deliberate,
    model-chosen exception to "every tool call here returns almost
    instantly," not an accident (see "How").
  - `cancel_task(task_id) -> String` — `kill`: if `Running`, marks the
    registry entry `Cancelled` and calls `.abort()` on the stored
    `AbortHandle`; a no-op (with a message saying so, not an error) if
    already `Finished`/`Failed`/`Cancelled`; `is_error: true` for an
    unknown id.

  Shared state: one in-memory task registry (e.g. `static TASKS:
  LazyLock<Mutex<HashMap<String, Task>>>` where `Task { conversation_id:
  i64, tool: String, status: TaskStatus, output: Vec<String>, result:
  Option<Result<String, String>>, abort: AbortHandle, notify:
  Arc<tokio::sync::Notify> }`, `TaskStatus = Running | Finished | Failed |
  Cancelled`) — written by `run_async`'s spawned task (status/result),
  `count` via the task-local (output), and `cancel_task` (status), read
  by every other tool in the suite. Deliberately in-memory and lost on
  restart, consistent with this file's already-provisional,
  deleted-once-real-tools-land status (see Open questions). `run_async`
  and the six task-management tools are the only genuinely reusable
  pieces here — everything else about this file (`add`, `count`) is
  still explicitly throwaway.
- **`src/anthropic/stream.rs`** — `interpret_stream_event`/`StreamOutcome`
  need to handle the events a tool-use turn actually emits, not just text
  deltas: `content_block_start` (capture `id`/`name` when the block is
  `tool_use`), `content_block_delta` of type `input_json_delta`
  (accumulate `partial_json`), `content_block_stop` (finalize the block —
  parse the accumulated JSON into `serde_json::Value`), and `message_delta`
  (capture `stop_reason`). `stream_anthropic_message`'s return type changes
  from `Result<String, String>` (assembled text) to `Result<StreamedTurn,
  String>` where `StreamedTurn { content: Vec<ContentBlock>, stop_reason:
  String }` — `on_delta` keeps firing for text deltas as today (live
  typing effect), tool-use blocks accumulate silently for this stage
  (streaming *their* activity live is a later plan's job). New tests: mock-upstream
  SSE bodies shaped like a real tool-use turn (`content_block_start` with
  `tool_use`, `input_json_delta` chunks, `content_block_stop`,
  `message_delta` with `stop_reason: "tool_use"`), plus the existing
  text-only path continuing to pass unchanged.
- **`src/api/chat.rs`** — `send_message` changes from one Anthropic call
  to a bounded loop (cap at 5 turns; exceeding it ends the turn with
  `ChatEvent::Error` rather than looping forever), exactly as in the
  original single-tool design: call `stream_anthropic_message`; if
  `stop_reason == "tool_use"`, persist the assistant's turn (its
  `ToolUse` block(s) included), run each tool through
  `anthropic::tools::execute(id, name, &input)` (the loop now has the
  conversation id in scope already, so threading it through to `execute`
  is a one-line change at every call site — `tools.rs` needs it, `chat.rs`
  just passes what it already has), persist the result(s) as a `role:
  "user"` message of `ToolResult` block(s) (standard Anthropic protocol
  shape — no new `role` value, no schema change to the `CHECK`
  constraint), append both to the in-loop history, and continue. On any
  other `stop_reason`, persist the final assistant turn and stop. **No
  special casing for any individual tool is needed here** — `run_async`,
  `list_tasks`, `task_status`, `task_output`, `task_result`, and
  `cancel_task` all return almost immediately, so their rounds look
  exactly like `add`'s from the loop's perspective. `wait_task` is the one
  exception: it can legitimately take up to its (clamped) timeout to
  return, so a turn that calls it holds this `send_message` request (and
  its SSE stream) open for that long — a deliberate, bounded exception the
  loop doesn't need to know about either, since from `chat.rs`'s point of
  view it's still just one `execute` call that eventually returns a
  string. `ChatEvent` is unchanged (`Delta`/`Done`/`Error`); `Done` still
  just fires once per row persisted, same as the original design.

  This is a meaningful simplification versus wiring backgrounding through
  `chat.rs` directly (an earlier revision of this plan tried exactly
  that, with a dedicated ticker, a new `ChatEvent::Backgrounded`, and a
  per-conversation lock): pushing the async/sync split down into
  `run_async` itself means the request loop never needs to know a tool
  call might outlive the request, and nothing outside the request that
  owns a conversation ever writes to its history — so there's no
  concurrency hazard to guard against and no need for the loop to treat
  any tool differently from `add`, `wait_task`'s bounded blocking aside.
- **`src/db.rs`** — `create_message`'s signature changes from `content:
  &str` to `content: &[ContentBlock]`, serializing to JSON internally
  (`serde_json::to_string`) before the `INSERT`. Every existing call site
  and test (`db.rs`, `chat.rs`) that currently passes a plain `&str`
  updates to pass `&[ContentBlock::Text { text: "...".to_string() }]`.
- **`src/models.rs`** — add `impl Message { pub fn blocks(&self) ->
  Result<Vec<ContentBlock>, serde_json::Error> }` parsing the stored JSON;
  compiles on both targets since `serde_json` is an unconditional
  dependency. `Message.content` itself stays `String` at the struct/DB
  level (still one plain TEXT column) — only its *contents* change
  semantics, from raw text to JSON-serialized `Vec<ContentBlock>`.
- **`migrations/`** — one new migration backfilling existing rows: today's
  `messages.content` holds plain text; after this change it must hold
  `[{"type":"text","text":<old value>}]`. **Spike first** (see Open
  questions) whether SQLite's JSON1 functions (`json_object`, `json_array`)
  are available in this project's bundled sqlite before writing the
  migration as SQL; fall back to a one-time idempotent Rust backfill in
  `db::init()` if not.
- **`src/frontend/pages/chat.rs`** — render `message.blocks()` instead of
  raw `message.content`. `Text` blocks render as today. `ToolUse`/
  `ToolResult` render as a single plain line each for every tool,
  including the six task-management ones (e.g. `Called
  run_async({"tool":"count","input":{"target":5,"interval_seconds":2}})`
  / `Started task toolu_01Ab...`, `Called
  wait_task({"task_id":"toolu_01Ab...","timeout_seconds":5})` /
  `finished: Counted to 5`) — one uniform, deliberately minimal
  placeholder for all of them, not the diff/live-output or process-list
  UI a later plan might build. A `blocks()` parse error surfaces as a
  visible error in that message's slot (per `development-process.md`'s
  "surface fallback outcomes" rule), not a blank render. No new
  `ChatEvent` handling, no polling loop, and no compose-box changes are
  needed here — every tool round resolves within its own normal request
  (bounded, for `wait_task`), and nothing about a still-running or
  finished background task is visible in the transcript again until the
  model itself chooses to call one of the task-management tools in a
  later turn (see "How" for why that's the right tradeoff rather than a
  gap).
- **Docs** — `docs/models.md` (content is now JSON, not plain text;
  document `Message::blocks()`), `docs/api.md` (`tools` field on the
  request, `Done` firing multiple times per send, current endpoint table
  unaffected — no new `ChatEvent` variant with this design),
  `docs/architecture.md` ("Request flow" section needs the loop, not a
  single call; note that `run_async` lets a tool call outlive the request
  that made it, entirely inside `anthropic::tools` — the request/response
  and persistence model elsewhere is otherwise unchanged),
  `docs/projects/state.md` (move tool-use out of "Explicitly out of
  scope," describe `add`/`count` as stand-in tools and
  `run_async`/`list_tasks`/`task_status`/`task_output`/`task_result`/
  `wait_task`/`cancel_task` as a stand-in for a real async-tool
  mechanism, both still to come).

## How

**Wire shape.** Anthropic's tool-use turns are standard multi-turn
protocol, nothing smelt-specific: the assistant's turn can contain
`ToolUse` blocks alongside/instead of `Text`; the *next* request includes
a `role: "user"` message whose content is the matching `ToolResult`
block(s), keyed by `tool_use_id`. `stop_reason: "tool_use"` on a turn is
the signal to do this rather than show the turn to the user as final.

**The loop.** `send_message` keeps its existing shape (store the user
message, load history, build a request, stream via `ServerEvents`) but the
single `stream_anthropic_message` call becomes a `for _ in 0..MAX_TURNS`
loop that mutates a local `Vec<AnthropicMessage>` (seeded from history) and
persists+emits after each turn, breaking out once `stop_reason` isn't
`"tool_use"`.

**Tool execution.** `anthropic::tools::execute` runs entirely in-process.
Called directly, `count` blocks for the full duration (a real, if small,
wait) — proving that some tools genuinely aren't instant. `add`, `count`,
`run_async`, and the task-management suite are chosen specifically
because they have zero security surface, so this stage can prove the
*protocol* mechanics (schema, `input` JSON round-trip, `tool_use_id`
matching, persistence, rendering) and the *async-wrapper* mechanics (one
tool's call spawning another tool's real execution on a background task,
managed afterward through a small process-like tool suite) without also
having to design sandboxing at the same time.

**Why the follow-up tools look like process management.** "List,
inspect, wait on, or stop a thing that's running somewhere" is exactly
what `ps`/`wait`/`kill` already solve, and reusing that shape means the
model isn't being asked to learn a bespoke smelt-specific vocabulary for
something it's almost certainly seen described the same way in code and
docs it was trained on: `run_async` is `fork`+`exec` (start it, get a
handle back immediately); `list_tasks` is `ps` (what's running, scoped to
this conversation the way `ps` is scoped to a session); `task_status` is
a non-blocking `ps <pid>`; `task_output` is `tail`; `task_result` is
reading the exit value once collected; `wait_task` is `waitpid` (or
`select`/`poll`) with a timeout; `cancel_task` is `kill`. None of these
need to be invented as smelt-specific concepts — the design cost here is
almost entirely in `run_async` and the shared registry; the six
management tools are each a thin, mechanical read (or one mutation, for
`cancel_task`) against that registry.

**Persistence.** One DB row per Anthropic-protocol message, matching
Anthropic's own transcript shape exactly (assistant turn with `ToolUse`,
user turn with `ToolResult`, final assistant turn) — no denormalization,
no "pack multiple turns into one row" cleverness, and — unlike an earlier
revision of this plan — no new kind of row or synthetic turn at all. Every
row is still either a real user message or a real assistant turn/tool
round; a still-running `run_async` task exists only in
`anthropic::tools`' in-memory registry, not in the conversation's history,
until the model chooses to ask about it via a `task_*` tool call (which
*does* produce ordinary `ToolUse`/`ToolResult` rows, same as any other
tool call).

**Why `run_async` is a wrapper tool instead of `count` being "async" on
its own.** Anthropic's Messages API has no concept of a streaming or
repeated tool result — a `tool_use` block gets exactly one matching
`tool_result` block, ever, in the very next `user` turn, and that closes
the exchange. There is no wire-level way to say "here's an interim result
for a call I haven't resolved yet," and nothing pushes information into a
conversation except a new model invocation. (The one adjacent concept,
server-side tools pausing with `stop_reason: "pause_turn"`, is Anthropic's
own infrastructure timing out mid-loop on its *own* tools — not
applicable here, since every tool in this plan is an ordinary client-side
tool smelt implements itself.) Given that, there are only two honest ways
to make *any* tool call "not block the conversation": either every
individual tool has to know how to detach itself and report back later
(what the previous revision of this plan tried with a `count`-specific
job map and a proactive "nudge" mechanism — see git history), or exactly
one generic wrapper tool knows how to detach *any* tool call, and the
wrapped tool itself stays completely ordinary. `run_async` is the latter:
it's the only place in this codebase that spawns a task and returns
before the real work is done; `count` doesn't know it's being wrapped
unless it explicitly checks (see below), and every other current or
future synchronous tool gets the same treatment for free just by being
nameable in `run_async`'s `tool` argument.

**How progress becomes observable without changing `execute`'s signature
any further.** `run_async`'s spawned task enters a `tokio::task_local!`
scope keyed by the task id before calling `execute(conversation_id, tool,
input)`. A tool implementation that wants to be observable while wrapped
— for this stage, only `count` — checks whether that task-local is set
and, if so,
appends a line to the task's output log in the shared registry after
each internal step; called directly (no `run_async`, no task-local in
scope), the exact same code takes the "not wrapped" branch and just
returns its final value with nothing recorded anywhere. This is
deliberately opt-in per tool rather than a generic instrumentation layer
`execute` forces on every tool — `add` never bothers checking, and that's
fine, since it has nothing worth reporting mid-flight anyway.

**What "notified of results" means without a push channel.** Once
`run_async`'s own tool_use/tool_result round closes (immediately, like
`add`'s), the conversation is in a completely ordinary, unblocked state —
same as after any other tool call — and nothing about a still-running
task causes a new turn to happen on its own; the model finds out about
progress only by choosing to call a tool. `wait_task` is what makes that
feel like being "notified" rather than "polled": the model calls it once
and, from its perspective, gets an answer only once something actually
changed (or the timeout it specified elapses) — it doesn't have to poll
in a tight loop itself. Concretely, `wait_task` checks `TASKS`' status
for `task_id` first; if already resolved, it returns immediately (no
different from `task_status`+`task_result`); otherwise it clones that
task's `Arc<Notify>` and races it against a `tokio::time::sleep
(timeout_seconds)` with `tokio::select!` — checking status *before*
subscribing to the notification is what avoids a lost-wakeup race (if the
task finished and fired `Notify` in the gap between "not yet resolved"
and "start waiting," a naive wait-without-checking-first would hang until
the timeout even though the answer was already sitting in the registry).
If the task outlives the timeout, `wait_task` returns a "still running"
status rather than blocking further — the model can call it again (a
bounded poll-with-backoff pattern) if it wants to keep waiting on
something that might run longer than one call's timeout allows.

The honest limits of this: nothing here pushes to the *browser*
independent of the model — a finished task the model never asks about
still shows up nowhere in the transcript, and the human watching the
chat has no separate signal. That's an accepted gap for this stage, not
solved here (see Open questions) — a later plan could add a lightweight
UI-only status poll against the task registry, decoupled entirely from
whether the model has been told.

**Cancellation.** `cancel_task` marks the registry entry `Cancelled`
*before* calling `.abort()` on the stored `AbortHandle`, rather than
relying on the aborted task to update its own state — an aborted
`tokio::spawn` task is dropped at its next `.await` point and never gets
to run any "I finished (by being killed)" cleanup code, so the registry
write has to happen from the cancelling side. This creates a small,
accepted race if the task was genuinely about to finish on its own at the
same moment: whichever of "the task's own completion write" and
"`cancel_task`'s `Cancelled` write" reaches the `Mutex` first wins, and
the loser's information is discarded. Not synchronized further for this
stage (see Open questions) — acceptable for a demo counter, worth
revisiting before this is trusted with a cancellation that has real
side effects to unwind.

## Open questions / tradeoffs

- **Max tool-turn bound** — proposing 5 as a sane default (matches nothing
  in the idea doc, just a guess at "clearly enough for `add`, clearly not
  infinite"); flag for confirmation rather than treating as settled.
- **`ChatEvent::Done` firing multiple times per send** — works with today's
  frontend loop unchanged, but the streaming spinner (`is_streaming`)
  stays on across all turns of one send with no visual distinction between
  "assistant is thinking" and "a tool ran" — acceptable placeholder given a
  later plan owns real visibility, but worth confirming that's an
  acceptable gap for this stage rather than a UX regression to fix now.
- **`src/anthropic/tools.rs`'s lifespan** — written as a hardcoded
  dispatcher (`add`, `count`, `run_async`, `list_tasks`, `task_status`,
  `task_output`, `task_result`, `wait_task`, `cancel_task`), not a
  registry, on the assumption `add` and `count` are deleted once a later
  plan lands real tools. The task-management suite is less obviously
  throwaway than `add`/`count` were — it's a genuine (if minimal) generic
  async-tool mechanism, not just a protocol smoke test — so flag
  explicitly whether it's expected to survive into the sandboxing plan
  essentially as-is (wrapping a real shell/build tool instead of `count`)
  or whether that plan is expected to redesign it from scratch once real
  tools raise requirements this stage doesn't fully solve (durable task
  state, output size limits, concurrent-task caps — see below).
- **In-memory task registry has no crash recovery** — if the server
  restarts while a `run_async` task is running, the registry entry (and
  the `tokio::spawn` task itself, and anything a caller is blocked in
  `wait_task` waiting on) is simply gone: any later call against that id
  errors as "unknown task id," same as a typo would. This does **not**
  leave any conversation stuck — `run_async`'s own tool_use/tool_result
  already closed normally — it just silently loses whatever that one
  task would have reported. Acceptable for a stage whose only wrappable
  tool is a capped counter; a real background tool (a shell command, a
  build) would need task state durable enough to survive a restart, or at
  least a way for the model to learn "that task no longer exists" rather
  than getting the same error as an invalid id.
- **No cap on concurrent tasks or task registry growth** — nothing stops
  the model from calling `run_async` many times (in one turn via
  Anthropic's parallel-tool-call support, or across turns; `list_tasks`
  makes this visible but doesn't limit it), and finished/cancelled tasks
  are never evicted from the in-memory map, so a long enough conversation
  accumulates unbounded entries. Fine at this stage's scale; flag whether
  a later revision needs a cap on concurrent tasks per conversation, a
  TTL/eviction policy for resolved ones, or both before this is trusted
  with anything less trivial than `count`.
- **`task_output`'s cursor model** — `task_output` returns the *entire*
  accumulated log every time rather than "output since I last checked."
  Fine for `count`'s handful of short lines; would need pagination or a
  since-cursor once a wrapped tool can produce a large or unbounded
  amount of output (e.g. a real long-running build) — noted as a gap the
  next revision of this mechanism should address, not solved here.
- **`wait_task`'s timeout bound, and repeated-wait cost** — proposing
  `timeout_seconds` clamped to `1..=10` as a sane default (a guess, like
  `MAX_TURNS` and `count`'s own clamps — flag for confirmation, not
  settled): long enough to catch a `count` job in one call, short enough
  that a single tool call can never turn into an effectively-unbounded
  hold on the `send_message` request. A task that outlives one
  `wait_task` call requires the model to call it again, each time holding
  the request open for up to another `timeout_seconds` — fine for a demo,
  but worth flagging that "wait" here is closer to "poll with a
  cooperative pause" than a true one-shot blocking wait for anything that
  might run longer than the clamp.
- **`cancel_task`'s race with natural completion** — as described in
  "How," a task finishing on its own at the same moment it's cancelled
  can have either write win the shared registry entry, with no
  synchronization beyond the `Mutex`'s per-field atomicity. Not resolved
  here; acceptable for a demo counter with no real side effects to unwind,
  but flag whether a later revision needs the two to be properly
  ordered (e.g. cancellation always losing to an already-in-flight
  completion) before `cancel_task` is used against a tool that does
  something real.
- **`list_tasks`' scope and conversation lifecycle** — tasks are scoped to
  `conversation_id` so one conversation can't see or cancel another's, but
  nothing here addresses what should happen to a task still running when
  its owning conversation is deleted (`delete_conversation` already
  exists — see `docs/api.md`). Proposing "nothing special" for this
  stage (the task keeps running to completion or its own natural end,
  orphaned, until the process restarts) rather than wiring
  `delete_conversation` to `cancel_task` every task it owns; flag whether
  that's an acceptable gap or a correctness issue worth closing now.
- **The browser has no independent signal** — as described in "How," a
  finished background task the model never asks about still shows up
  nowhere in the transcript; a human watching the chat has to wait for
  the model to mention it. `wait_task` gives the *model* something better
  than raw polling, but doesn't give the *UI* anything at all. Flag
  whether that's acceptable for this stage or whether a later plan should
  add a lightweight UI-only status poll against the task registry,
  decoupled entirely from whether the model has been told.
