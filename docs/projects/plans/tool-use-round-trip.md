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
- **`src/anthropic/tools.rs` (new)** — the one trivial tool for this
  stage: `add(a, b) -> f64`. A `ToolDefinition` constant/fn plus a
  `execute(name: &str, input: &serde_json::Value) -> Result<String,
  String>` dispatcher. Explicitly provisional — deleted once a later plan
  ships real tools; not designed to grow into a tool registry yet (that's
  a concern for whenever there's more than one tool).
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
- **`src/api/chat.rs`** — `send_message` changes from one Anthropic call to
  a bounded loop (cap at 5 turns; exceeding it ends the turn with
  `ChatEvent::Error` rather than looping forever): call
  `stream_anthropic_message`; if `stop_reason == "tool_use"`, persist the
  assistant's turn (its `ToolUse` block(s) included), run each tool
  through `anthropic::tools::execute`, persist the result(s) as a
  `role: "user"` message of `ToolResult` block(s) (this is standard
  Anthropic protocol shape — no new `role` value, no schema change to the
  `CHECK` constraint), append both to the in-loop message history, and
  call `stream_anthropic_message` again. On any other `stop_reason`,
  persist the final assistant turn and stop. `ChatEvent` itself is
  unchanged (`Done { message_id, content: String }`) — it now simply fires
  once per row persisted during the loop (1 for a plain reply, 3 for one
  tool round: assistant-with-tool-use, tool-result, assistant-final).
  Existing frontend code that loops `while let Some(event) = events.recv()`
  already handles repeated `Done` events with no changes needed there.
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
  `ToolResult` render as a single plain line each (e.g. `Called
  add({"a":1,"b":2})` / `→ 3`) — a deliberately minimal placeholder, not
  the diff/live-output rendering that's a later plan's job. A `blocks()` parse
  error surfaces as a visible error in that message's slot (per
  `development-process.md`'s "surface fallback outcomes" rule), not a
  blank render.
- **Docs** — `docs/models.md` (content is now JSON, not plain text;
  document `Message::blocks()`), `docs/api.md` (`tools` field on the
  request, `Done` firing multiple times per send, current endpoint table
  unaffected), `docs/architecture.md` ("Request flow" section needs the
  loop, not a single call), `docs/projects/state.md` (move tool-use out of
  "Explicitly out of scope," describe the trivial `add` tool as a stand-in
  for real tools still to come).

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

**Tool execution.** `anthropic::tools::execute` runs entirely in-process,
synchronously, no I/O — `add` is chosen specifically because it has zero
security surface, so this stage can prove the *protocol* mechanics
(schema, `input` JSON round-trip, `tool_use_id` matching, persistence,
rendering) without also having to design sandboxing at the same time.

**Persistence.** One DB row per Anthropic-protocol message, matching
Anthropic's own transcript shape exactly (assistant turn with `ToolUse`,
user turn with `ToolResult`, final assistant turn) — no denormalization,
no "pack multiple turns into one row" cleverness.

## Open questions / tradeoffs

- **SQLite JSON1 availability** — needs a spike before the migration is
  written (see above). If unavailable, the backfill moves to Rust and the
  migration file only needs to exist if there's a schema change to make
  (there isn't one — `content` stays `TEXT`), so in that fallback case
  there may be no new migration file at all, just a startup backfill.
- **Max tool-turn bound** — proposing 5 as a sane default (matches nothing
  in the idea doc, just a guess at "clearly enough for `add`, clearly not
  infinite"); flag for confirmation rather than treating as settled.
- **`ChatEvent::Done` firing multiple times per send** — works with today's
  frontend loop unchanged, but the streaming spinner (`is_streaming`)
  stays on across all turns of one send with no visual distinction between
  "assistant is thinking" and "a tool ran" — acceptable placeholder given a
  later plan owns real visibility, but worth confirming that's an
  acceptable gap for this stage rather than a UX regression to fix now.
- **`src/anthropic/tools.rs`'s lifespan** — written as a single hardcoded
  tool, not a registry, on the assumption it's deleted (not extended) once
  a later plan lands real tools. If that assumption is wrong and `add` (or
  something like it) should stick around as a permanent smoke-test tool,
  the "delete it later" note in "Which files" above should change to
  "keep it alongside real tools."
