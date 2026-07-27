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
the task finishes or the timeout elapses, for when the model wants to
pause and wait inline; and `cancel_task` (`kill`) best-effort aborts a
still-running one.

On top of that pull-based suite, a task now also gets pushed to the
model automatically: when it reaches a terminal state (finished, failed,
or cancelled), the harness injects a new conversation turn reporting the
result — no `task_*` tool call required to find out. `run_async` also
takes an optional `stream_output` flag; when set, every line the wrapped
tool writes (its "stdout/stderr," in this design just the same
task-local output-log convention `count` already uses) is pushed as its
own turn as soon as it's produced, not just batched into `task_output`
for a later pull. Anthropic's protocol still has no way to push *into* an
in-flight model response — there's no live connection to interrupt — so
both of these are implemented the same way: the harness manufactures an
ordinary new turn on the conversation, the same as if a user (or a
previous revision's discarded "nudge" mechanism) had sent a message. See
"How" below for why this is the only honest way to do it, and the
concurrency/cost consequences of doing it per-line.

The browser gets the same "immediately, no polling" treatment, but
through a different mechanism, since a *human* watching a chat tab has
no equivalent of "the model's own tool call" to hang a live update off
of — nothing is stopping them from just staring at the screen while a
task runs. This plan adds a dedicated, always-open per-conversation event
stream — independent of any particular `send_message` call, since task
activity (a tick, a finish) can happen with no request in flight at all —
that pushes two kinds of thing to the browser the moment they happen: a
lightweight status ping for every task lifecycle event (started, each
output line, finished/failed/cancelled) purely for UI feedback, and the
same newly-persisted conversation rows the model-facing push mechanism
above produces, so other open tabs (or a tab that wasn't the one that
started the task) see them land without needing to refresh. This replaces
the "emit `ChatEvent::Backgrounded`, then poll `get_messages` and guess
when to stop" mechanism from the previous revision entirely — a real live
stream is a strictly better fit for "a browser tab that's just sitting
there, watching," and removes the guessing. See "How" for why this needs
its own stream rather than reusing `send_message`'s.

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

- **`src/events.rs` (new)** — the shared home for the per-conversation
  live event bus, so neither `tools.rs` nor `chat.rs` has to depend on
  the other to publish to or read from it. `ConversationEvent { TaskUpdate
  { task_id: String, tool: String, status: String, latest_output:
  Option<String> }, MessagesAppended(Vec<Message>) }`; a lazily-created
  `static BUSES: LazyLock<Mutex<HashMap<i64, broadcast::Sender
  <ConversationEvent>>>>` (`tokio::sync::broadcast`, not `Notify` or an
  mpsc — broadcast is the one that supports multiple simultaneous
  subscribers per conversation, which matters once more than one browser
  tab can watch the same conversation); `fn publish(conversation_id: i64,
  event: ConversationEvent)` (creates the sender if this is the first
  event for that id; a no-op, cost-wise, if nobody's subscribed —
  `broadcast::Sender::send` on a channel with zero receivers just drops
  the value) and `fn subscribe(conversation_id: i64) ->
  broadcast::Receiver<ConversationEvent>` (creates the sender if this is
  the first *subscriber*, i.e. either side can be first). `tools.rs`
  calls `publish`; `chat.rs` calls both.
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
  - `run_async(tool, input, stream_output = false) -> String` — the
    generic wrapper, `fork`+`exec` for tool calls. Validates `tool` names
    a real, non-`run_async`, non-`task_*` tool (rejects nesting and
    unknown names immediately, `is_error: true`, no task spawned), then
    `tokio::spawn`s a task that sets a task-local current-task-id context
    (e.g. `tokio::task_local! { static CURRENT_TASK: String }`, entered
    via `CURRENT_TASK.scope(task_id.clone(), execute(conversation_id,
    tool, input)).await`), stores the spawned task's `AbortHandle` (for
    `cancel_task`) and a `tokio::sync::Notify` (for `wait_task`) alongside
    a new registry entry recording `stream_output`, and calls `execute`
    inside that scope. Also calls `events::publish(conversation_id,
    TaskUpdate { status: "running", latest_output: None, .. })`
    immediately, so the browser sees "started" the instant the task
    exists, before it's produced anything. Three things now happen around
    the `execute` call that didn't before: every time `count` (or any
    future observable tool) emits a line, `events::publish`'s a
    `TaskUpdate` carrying that line — **unconditionally**, regardless of
    `stream_output`, since this is a cheap in-process broadcast, not an
    API call; *additionally*, only if `stream_output` is true, that same
    line is pushed to the conversation as its own real turn (see `chat.rs`
    and "How" — this is the expensive, model-facing path, deliberately
    gated separately from the free browser-facing one); and when
    `execute` resolves — Finished or Failed, and `cancel_task`'s abort
    path is the third way in — the harness *always* both publishes a
    terminal `TaskUpdate` (browser) and pushes one final "task done" turn
    (model) carrying the result or error, regardless of `stream_output`.
    The task id reuses the `run_async` tool_use's own `id` — same "id
    already exists, don't mint a new one" pattern used elsewhere in this
    plan. `run_async` itself still returns immediately with a short
    string naming the task id and pointing at the rest of the suite (e.g.
    `"Started task toolu_01Ab... running count; use list_tasks/
    task_status/task_output/task_result/wait_task/cancel_task with this
    id — you'll also be notified here when it finishes."`).
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
    `AbortHandle` — and, since an aborted task is dropped mid-`.await` and
    never reaches the "I'm done, push a notification" code the *normal*
    completion path relies on, `cancel_task` itself pushes the "task
    cancelled" notification turn (see `chat.rs`/"How"), the same way a
    natural finish or failure would have. A no-op (with a message saying
    so, not an error) if already `Finished`/`Failed`/`Cancelled`;
    `is_error: true` for an unknown id.

  Shared state: one in-memory task registry (e.g. `static TASKS:
  LazyLock<Mutex<HashMap<String, Task>>>` where `Task { conversation_id:
  i64, tool: String, stream_output: bool, status: TaskStatus, output:
  Vec<String>, result: Option<Result<String, String>>, abort:
  AbortHandle, notify: Arc<tokio::sync::Notify> }`, `TaskStatus = Running
  | Finished | Failed | Cancelled`) — written by `run_async`'s spawned
  task (status/result), `count` via the task-local (output), and
  `cancel_task` (status), read by every other tool in the suite.
  Deliberately in-memory and lost on restart, consistent with this file's
  already-provisional, deleted-once-real-tools-land status (see Open
  questions). `run_async` and the six task-management tools are the only
  genuinely reusable pieces here — everything else about this file
  (`add`, `count`) is still explicitly throwaway.

  One more function, not a tool the model calls: `pub fn
  snapshot_tasks(conversation_id: i64) -> Vec<TaskSummary>` reads the
  same `TASKS` registry and returns a plain snapshot (`{task_id, tool,
  status}` per entry) for the *browser* — the thing `list_tasks` does for
  the model, exposed read-only to `chat.rs`'s new browser-facing endpoint
  (below) too, sharing one implementation rather than two copies of the
  same filter-by-`conversation_id` logic.
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
- **`src/api/chat.rs`** — the per-request loop stays almost exactly as
  before, plus one piece of machinery this plan removed last revision and
  now needs back, generalized.

  **1. The turn loop (unchanged in shape).** `send_message` changes from
  one Anthropic call to a bounded loop (cap at 5 turns; exceeding it ends
  the turn with `ChatEvent::Error` rather than looping forever): call
  `stream_anthropic_message`; if `stop_reason == "tool_use"`, persist the
  assistant's turn (its `ToolUse` block(s) included), run each tool
  through `anthropic::tools::execute(id, name, &input)` (the loop already
  has the conversation id in scope, so threading it through is a one-line
  change at each call site), persist the result(s) as a `role: "user"`
  message of `ToolResult` block(s) (standard Anthropic protocol shape —
  no new `role` value, no schema change to the `CHECK` constraint),
  append both to the in-loop history, and continue. On any other
  `stop_reason`, persist the final assistant turn and stop. As before, no
  individual tool needs special-casing *inside* this loop — `run_async`
  and every `task_*` tool but `wait_task` return almost immediately;
  `wait_task` is the one deliberate, bounded exception that can hold this
  `send_message` request open for up to its timeout. This loop body is
  once again pulled out into a reusable async fn (call it `run_turn(id:
  i64, new_message: AnthropicMessage, on_delta: Option<&mut dyn
  FnMut(&str)>) -> ServerFnResult<Vec<Message>>`, loading history itself)
  — `send_message` calls it with the live `tx` wired into `on_delta`;
  the push mechanism below calls it with `on_delta = None`.

  **2. Pushing a turn from outside a request.** When `run_async`'s
  spawned task (in `tools.rs`) needs to notify the conversation — either
  because the wrapped tool emitted a line under `stream_output`, or
  because the task just resolved — it calls `chat::run_turn` directly,
  passing a synthetic `role: "user"` message that names the task id and
  tool (e.g. `<task-output task_id="toolu_01Ab..." tool="count">count:
  3/5</task-output>` for a streamed line, `<task-notification
  task_id="toolu_01Ab..." tool="count">finished: Counted to
  5</task-notification>` for the terminal one — content included, not
  withheld, since the point this time is telling the model what
  happened). Because `run_async`'s own spawned task is what's already
  producing these lines one at a time, in order, it `.await`s each
  `run_turn` call before moving on to the next tick — this keeps
  per-task notifications strictly ordered with no extra queue needed; it
  does *not* block `send_message`'s original request, which returned
  long before any of this happens, and it does *not* block other tasks'
  own notifications, which are running (and awaiting their own
  `run_turn` calls) on their own spawned tasks in parallel.

  **3. Concurrency.** A live `send_message` call and a task's
  push-triggered `run_turn` call can now genuinely race for the same
  conversation, and so can two different tasks' pushes for the same
  conversation — Anthropic's strict user/assistant alternation breaks if
  two writers persist a turn at once. `run_turn` takes a per-conversation
  async lock (`static CONVERSATION_LOCKS: LazyLock<Mutex<HashMap<i64,
  Arc<tokio::sync::Mutex<()>>>>>`, or equivalent) around history-load
  through persist, exactly as an earlier revision of this plan designed
  and then removed — it's back because genuine push, not just polling,
  requires it. Which caller acquires the lock first when several are
  ready is unspecified (see Open questions). Once `run_turn` finishes
  persisting a batch of rows — whether called from `send_message` or from
  a task's push — it calls `events::publish(id,
  ConversationEvent::MessagesAppended(rows))`, so every live subscriber
  (see point 4) gets them immediately, not just the request that
  triggered them.

  **4. A dedicated, always-open browser stream — no more
  `ChatEvent::Backgrounded` or polling.** New server function:
  `#[get] subscribe_conversation_events(id: i64) -> ServerFnResult
  <ServerEvents<ConversationEvent>>`. The frontend opens this once per
  viewed conversation (see `frontend/pages/chat.rs`) and it just forwards
  whatever `events::subscribe(id)` yields — `TaskUpdate`s and
  `MessagesAppended`s alike — for as long as the browser keeps the
  connection open. This fully replaces the previous revision's
  `ChatEvent::Backgrounded` + "start polling `get_messages` and guess when
  to stop" mechanism, which is removed: the browser no longer needs a
  signal telling it *when* to start looking for updates, because it's
  always already subscribed.
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
  raw `message.content`. `Text` blocks render as today, including the
  synthetic `<task-output>`/`<task-notification>`-tagged pushed messages
  — rendered as plain text like any other message for this stage
  (visually indistinguishable from something a human typed; flagged in
  Open questions, not solved here). `ToolUse`/`ToolResult` render as a
  single plain line each for every tool (e.g. `Called
  run_async({"tool":"count","input":{"target":5,"interval_seconds":2,
  "stream_output":true}})` / `Started task toolu_01Ab...`) — one uniform,
  deliberately minimal placeholder, not the diff/live-output or
  process-list UI a later plan might build. A `blocks()` parse error
  surfaces as a visible error in that message's slot (per
  `development-process.md`'s "surface fallback outcomes" rule), not a
  blank render.

  New: when a conversation is selected, open
  `subscribe_conversation_events(id)` (in addition to, and independent
  of, whatever `send_message` calls are in flight) and keep it open for
  as long as that conversation is selected, reconnecting (with a short
  backoff) if it drops. On connect/reconnect, first do a one-time
  reconciliation pull — `get_messages(id)` plus a new small `get_tasks(id)`
  server function (thin wrapper around `anthropic::tools::
  snapshot_tasks`) — since a `broadcast` channel has no replay and a
  reconnect could have missed events; this one-shot pull on connect is
  not the same thing as the polling loop being removed, since it runs
  once per connection, not on a timer. From there, `MessagesAppended`
  events append onto `messages` (deduped by message id, same
  already-established idiom as the optimistic-send pattern, since the
  same rows can arrive both via `send_message`'s own `ChatEvent::Done`
  and via this stream); `TaskUpdate` events feed a small, separate
  "background tasks" panel (task id, tool, status, latest output line) —
  not mixed into the message transcript, since these are UI telemetry,
  not conversation turns.
- **Docs** — `docs/models.md` (content is now JSON, not plain text;
  document `Message::blocks()`), `docs/api.md` (`tools` field on the
  request, `Done` firing multiple times per send, the new
  `subscribe_conversation_events`/`get_tasks` endpoints and the
  `ConversationEvent` shape, and that some rows can now be persisted by a
  background task rather than the request that looks like it "sent"
  them), `docs/architecture.md` ("Request flow" section needs the loop,
  not a single call; a conversation can now receive turns pushed from a
  source other than an incoming `send_message` call; a per-conversation
  lock serializes writers and a per-conversation broadcast bus
  (`events.rs`) fans out reads to any number of live subscribers),
  `docs/projects/state.md` (move tool-use out of "Explicitly out of
  scope," describe `add`/`count` as stand-in tools and
  `run_async`/`list_tasks`/`task_status`/`task_output`/`task_result`/
  `wait_task`/`cancel_task` — plus its push, streaming, and live-browser
  behavior — as a stand-in for a real async-tool mechanism, both still to
  come).

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
no "pack multiple turns into one row" cleverness. This does now include
synthetic pushed turns (`<task-output>`/`<task-notification>`-tagged
`role: "user"` `Text` rows the harness writes on a task's behalf,
followed by whatever the model says in response) — a design point this
plan tried to avoid one revision ago in favor of a pure-pull model, and
is reintroducing now because genuine push, not just a nicer poll, is
what's being asked for. Nothing about the row shape itself changes: a
pushed turn is stored exactly like a real user message, indistinguishable
in the schema (see Open questions for the rendering consequence of that).

**Why `run_async` is a wrapper tool instead of `count` being "async" on
its own.** Anthropic's Messages API has no concept of a streaming or
repeated tool result — a `tool_use` block gets exactly one matching
`tool_result` block, ever, in the very next `user` turn, and that closes
the exchange. There is no wire-level way to say "here's an interim result
for a call I haven't resolved yet," and nothing pushes information into a
conversation except a new model invocation — that fact hasn't changed
across any revision of this plan and isn't going to; it's a property of
the API, not a design choice made here. (The one adjacent concept,
server-side tools pausing with `stop_reason: "pause_turn"`, is Anthropic's
own infrastructure timing out mid-loop on its *own* tools — not
applicable here, since every tool in this plan is an ordinary client-side
tool smelt implements itself.) Given that, there are only two honest ways
to make *any* tool call "not block the conversation": either every
individual tool has to know how to detach itself and report back later
(what a since-superseded revision of this plan tried with a
`count`-specific job map — see git history), or exactly one generic
wrapper tool knows how to detach *any* tool call, and the wrapped tool
itself stays completely ordinary. `run_async` is the latter: it's the
only place in this codebase that spawns a task and returns before the
real work is done; `count` doesn't know it's being wrapped unless it
explicitly checks (see below), and every other current or future
synchronous tool gets the same treatment for free just by being nameable
in `run_async`'s `tool` argument. What *has* changed since that
superseded revision is that pushing a notification when something happens
is no longer optional or count-specific either — see below.

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

**Making "notified without running a tool" real: the harness resumes the
conversation on its own.** Since nothing can be pushed into an in-flight
model call, "notify the model when a task finishes, with no tool call
required" can only mean one thing: the harness starts a *new* turn on the
model's behalf, the same way a real user message would, carrying the news
as its content. Concretely, `run_async`'s spawned task (or `cancel_task`,
for the cancellation path) calls `chat::run_turn` with a synthetic
`role: "user"` message once the wrapped tool resolves — this is a real
Anthropic request, indistinguishable on the wire from any other turn, and
it produces a real assistant reply that gets persisted and streamed to
whatever's watching. Unlike the discarded revision-2 "nudge," this
notification includes the actual result or error inline rather than
withholding it — the point this time genuinely is a completion callback,
not a low-information ping the model has to follow up on. `task_result` /
`task_output` / `task_status` remain useful regardless, for a model
that's reviewing history later, whose context got compacted since the
push happened, or that just wants to double-check.

**Streaming mode: ordering and — importantly — cost.** With
`stream_output: true`, the same push mechanism fires once per line
instead of once at the end. Ordering is free: because the wrapped tool
(e.g. `count`) emits lines one at a time from within its own single
spawned task, and that task `.await`s each `run_turn` call before
producing the next line, there's exactly one writer per task and no
possibility of its own lines arriving out of order — no queue or sequence
number needed on top of the per-conversation lock. Cost is the real
concern and is **not** solved here: every line becomes a full
`stream_anthropic_message` round trip, so a `count(target=5,
interval_seconds=2, stream_output=true)` job costs at least 6 model calls
(one per tick plus the final notification) to prove a five-second demo.
That's acceptable at `count`'s clamped scale; it is very likely *not*
acceptable for any real streaming tool (a build or a shell command can
produce thousands of lines), where per-line pushes at this granularity
would be prohibitively expensive and slow. Flagged prominently in Open
questions — coalescing (e.g. push at most every N lines or every K
seconds) is almost certainly required before `stream_output` is used for
anything beyond a toy counter.

**Interaction with `wait_task`.** A model that calls `wait_task` on a
task that resolves while the call is in flight gets the answer twice:
once as `wait_task`'s own return value in the current turn, and again —
independently — via the completion push that fires for every task
regardless of who else is watching it. `wait_task`'s `tokio::select!`
and the completion push both react to the same `Notify`/registry write,
but neither knows about the other, so there's no attempt to suppress the
redundant one. Harmless (a mildly odd "told you twice" in the transcript)
but not silently free of the completion-push cost from the model's
perspective — flagged, not resolved, in Open questions.

**Why the browser needs its own stream instead of reusing
`send_message`'s.** `send_message`'s `ServerEvents<ChatEvent>` is scoped
to one HTTP request: it exists because a browser tab just sent a message
and wants to watch that specific reply stream in. Task activity has no
such request to hang off of — `run_async` already returned by the time
anything interesting happens, and a browser tab that never sent a message
at all (someone just watching, or a second tab open on the same
conversation) has no `send_message` call to attach to in the first place.
So `chat.rs` exposes a second, independent `ServerEvents` endpoint,
`subscribe_conversation_events`, that a tab opens once per conversation
and keeps open for as long as it's looking at it — not tied to sending
anything. Both endpoints are the same underlying Dioxus primitive
(`ServerEvents`); they just answer different questions ("stream me the
reply to what I just sent" vs. "stream me whatever happens to this
conversation, unprompted").

**Why `broadcast`, and why a reconciliation pull still exists despite
"no more polling."** `tokio::sync::broadcast` is the one channel type
built for "zero or more live subscribers, no history" — exactly this
shape, and it drops a value cleanly if nobody's listening rather than
buffering unboundedly. Its cost is the flip side of that: a subscriber
that wasn't connected when an event fired never sees it — no replay.
That's fine for `TaskUpdate` (an ephemeral UI ping; missing one tick of a
counter is a cosmetic gap, not a correctness one), but not fine for
`MessagesAppended`, since those rows are the actual durable conversation
— a browser tab that reconnects after a drop needs to end up
*consistent*, not just told about whatever happens to arrive next. That's
why `subscribe_conversation_events` is paired with one `get_messages` +
`get_tasks` pull at connect time: the live stream carries everything from
that point forward, and the one-time pull fills in whatever came before
it connected. This is a fetch-once reconciliation, not a polling loop —
it runs once per connection (initial load, or reconnect after a drop),
never on a repeating timer.

**`TaskUpdate` is ephemeral; `MessagesAppended` rides on already-durable
data.** `TaskUpdate` events are never persisted anywhere — they exist
only as broadcast messages, generated from (and always re-derivable from,
via `get_tasks`/`list_tasks`) the same in-memory task registry that
already has no crash-recovery story (see Open questions). `MessagesAppended`
carries no new data of its own; it's just a live delivery notification for
rows that `run_turn` already persisted to the database. Losing a
`TaskUpdate` broadcast costs a moment of stale-looking UI; losing a
`MessagesAppended` broadcast costs nothing beyond a slightly later
`get_messages` pull picking up the same row on the next connection.

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
- **Per-conversation broadcast bus lifecycle** — `events::BUSES` never
  removes an entry once created, same shape as the existing "no eviction"
  gap for the task registry. A `broadcast::Sender` with zero receivers is
  cheap to keep around (sends just get dropped), so this is lower-stakes
  than the task registry's growth concern, but it's still an unbounded map
  keyed by every conversation id that's ever had a subscriber or a
  publish — flag whether a later revision needs to prune entries for
  deleted conversations (ties to the same `delete_conversation` question
  above) or reference-count them down when both all subscribers and all
  tasks for a conversation are gone.
- **Reconnect/backoff policy is unspecified** — the frontend bullet says
  "reconnect with a short backoff" without a concrete policy (fixed
  interval? exponential? a cap on retries?). Proposing a simple fixed
  short delay (e.g. 1–2s) as good enough for this stage, matching the
  "guessed bound, flag for confirmation" treatment given to `MAX_TURNS`
  and `count`'s clamps elsewhere in this plan — not meant to be a
  production reconnect strategy.
- **Gaps in `TaskUpdate` during a disconnect are silent** — as described
  in "How," this is accepted for cosmetic UI ticks, but there's no signal
  to the user that *some* updates were missed (e.g. "reconnected, may have
  missed some progress" vs. a UI that looks fully caught-up when it might
  not show every intermediate tick). `list_tasks`/`get_tasks` still gives
  an accurate *current* snapshot on reconnect, so nothing is ever
  permanently wrong — just potentially missing some of the "as it
  happened" texture. Flag whether that distinction needs to be surfaced
  to the user or is fine left implicit.
- **Streaming mode's per-line cost has no mitigation** — as described in
  "How," every streamed line is a full model round trip; this is
  workable only because `count`'s target/interval are both clamped to a
  handful. Flag whether `run_async`'s `stream_output` option should ship
  at all before a coalescing strategy (batch N lines, or a minimum
  interval between pushes) exists — as designed, turning it on for
  anything less trivial than `count` would be closer to a denial-of-wallet
  than a feature.
- **Double notification when `wait_task` and a completion push race** —
  as described in "How," a model that's mid-`wait_task` when the task it's
  waiting on resolves hears about it twice: once as `wait_task`'s return
  value, once via the independent completion push. Not suppressed; flag
  whether that's harmless enough to leave (the redundant push is at least
  truthful, just wasteful) or whether `wait_task` should mark the task as
  "notification already delivered" so the completion-push path skips it.
- **Lock ordering when several writers are ready for one conversation** —
  now three kinds of caller can be waiting on the same per-conversation
  lock at once: a live `send_message`, a task's per-line push (streaming
  mode), and a task's completion push. The lock guarantees they're
  serialized, not corrupted, but not in any particular order. Proposing
  "whichever acquires first" as the simplest correct behavior for this
  stage, same as the prior revision that first introduced this lock
  proposed and then removed it; flag if a later plan wants live user
  messages to always preempt a pending push (or the reverse).
