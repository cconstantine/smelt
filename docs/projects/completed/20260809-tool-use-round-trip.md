# Tool-use round trip

**Branch:** `tool-use-round-trip` · **Idea:** `projects/ideas/coding-session.md` (kept — this was the first of several plans against it, sandboxing still to come) · **Plan:** `projects/plans/tool-use-round-trip.md` (removed)

## What shipped

The full plan: the Anthropic tool-use protocol round-trips through this
codebase end to end, plus a second execution shape — a generic
`run_async` mechanism for running any tool call in the background, with a
`ps`/`wait`/`kill`-style management suite and live push notifications.

- **Wire types** (`anthropic::types`): `ContentBlock::ToolUse`/`ToolResult`,
  `ToolDefinition`, `CreateMessageRequest.tools`.
- **Persistence** (`models.rs`, `db.rs`, a new migration): `Message.content`
  is now JSON-serialized `Vec<ContentBlock>`, not plain text —
  `Message::blocks()` parses it back; `db::create_message` takes
  `&[ContentBlock]`; a backfill migration converted existing rows.
- **Streaming** (`anthropic::stream`): `interpret_stream_event` handles
  `content_block_start`/`input_json_delta`/`content_block_stop`/
  `message_delta`; `stream_anthropic_message` returns a `StreamedTurn`
  (content blocks + stop reason) instead of an assembled string.
- **Two throwaway tools** (`anthropic::tools`): `add` and `count` —
  `count` is deliberately slow (real `tokio::time::sleep` between
  increments), chosen specifically to have something worth backgrounding.
- **The turn loop** (`api::chat::run_turn`): `send_message` is now a thin
  wrapper around a bounded (`MAX_TURNS`) loop that persists each
  assistant/tool-result turn and executes tools via
  `anthropic::tools::execute`. Pulled out as a boxed, type-erased future
  (not `async fn` sugar) specifically because it and `execute` call each
  other, which defeats rustc's `Send` auto-trait inference for plain
  `async fn`s.
- **`run_async` + the task suite** (`anthropic::tools`): `run_async`
  spawns a wrapped tool call on a background task and returns immediately;
  `list_tasks`/`task_status`/`task_output`/`task_result`/`wait_task`/
  `cancel_task` manage it afterward. An in-memory registry
  (`static TASKS`), a `tokio::task_local!` so `count` can tell whether
  it's running wrapped, and a `tokio::sync::Notify` per task for
  `wait_task`.
- **Push notifications**: a background task's spawned code calls
  `chat::run_turn` directly (with `on_delta = None`) to push a synthetic
  `role: "user"` turn into the conversation — per streamed line (if
  `stream_output: true`) and always once, terminally, on
  finish/fail/cancel. A per-conversation async lock
  (`CONVERSATION_LOCKS`) serializes every writer, since a live request and
  a background push can now both want to persist a turn at once.
- **Live browser stream** (`events.rs`, `subscribe_conversation_events`,
  `get_tasks`): a per-conversation `tokio::sync::broadcast` bus;
  `ChatPanel` opens a live subscription per selected conversation
  (web-only), does a one-shot `get_messages`/`get_tasks` reconciliation
  pull on connect/reconnect, dedupes appended messages by id, and renders
  a separate background-tasks panel.
- New dependency: `gloo-timers` (web-only) for the reconnect backoff —
  `tokio::time::sleep` has no driver in a browser tab, since there's no
  tokio runtime there at all.

Docs updated: `models.md`, `migrations.md`, `api.md`, `architecture.md`,
`testing.md`, `projects/state.md`.

## UI polish (background-tasks panel)

A follow-up pass, once live-testing (see below) made the first version's
UI gaps obvious: the background-tasks panel was a single-line-per-task
strip wedged between the transcript and the composer, `run_async` calls
rendered as a full call-card plus a redundant "task started" result card,
and neither the transcript nor the panel tracked scroll position at all.

- **Terminal-styled task widgets** (`TaskPanelEntry`, `assets/chat.css`):
  each background task now renders as its own dark, titlebar'd widget
  (tool name, task id, status pill, monospace scrollback) instead of a
  status row — explicitly shaped to grow into a real interactive shell
  session later, not just a log viewer. Required extending
  `anthropic::tools::TaskSummary` to carry the task's full accumulated
  `stdout`/`stderr` (previously status-only), so a page load or SSE
  reconnect can hydrate complete scrollback rather than only whatever
  streamed in live from that point forward.
- **Sidebar layout**: the panel moved from a horizontal strip above the
  composer to a fixed-width column beside the conversation (`chat-main` +
  `tasks-panel` as row-flex siblings inside `.chat-panel`), so it no longer
  competes with the transcript for vertical space.
- **Compact `run_async` calls**: a `ToolUse{name: "run_async"}` block now
  renders as a single collapsed line ("Started `count`") via a native
  `<details>` disclosure instead of a full call card, and its matching
  `ToolResult` — always the same generic "task started" boilerplate — is
  suppressed entirely (`tool_use_names_by_id` maps a `ToolResult`'s
  `tool_use_id` back to the tool name that started it, since the block
  itself doesn't carry one). The tasks sidebar is the real place that
  shows what a background task is doing; the inline card was redundant
  with it.
- **Sticky-bottom auto-scroll**: the transcript and each task's terminal
  body independently track whether the user is scrolled to the bottom
  (`onscroll`, a small `is_scrolled_to_bottom` slack check) and, only if
  so, follow new content down (`onmounted` + `MountedData::get_scroll_size`/
  `.scroll()` — Dioxus's native element-handle API, no hand-rolled JS).
  Scrolling up to read history is left alone rather than yanked back down.
- **Playwright added to the dev container** (`Dockerfile`,
  `/opt/playwright-venv`) specifically to make this pass possible — see
  retrospective.

Docs updated further: `testing.md` (Playwright as the preferred browser
tool, `scripts/browser-check/` demoted to documented fallback).

## Retrospective

**What worked:**
- Live-testing against the real Anthropic API (via `dx serve --fullstack`
  + `curl` against the actual SSE endpoints, since no browser automation
  was available in this sandbox) caught two real bugs unit tests alone
  missed entirely — see below. Worth treating as a mandatory step for any
  change that touches the tool-calling loop, not just a nice-to-have.
- The plan's own "Open questions" section correctly anticipated most of
  the hard tradeoffs (guessed clamps, `stream_output` cost, the
  cancel/completion race) — implementation mostly followed the plan as
  written, deviating only where testability forced a concrete design
  choice the plan had left abstract (see below).
- TDD on the pure logic (block rendering, message/task-list merging, the
  wire-format parsing) worked cleanly and caught real regressions early —
  e.g. a stale `test_interpret_non_text_delta_is_ignored` that contradicted
  the new `input_json_delta` handling before it was renamed to test what
  it now actually should.

**What caused friction / surprises:**
- **`run_turn` and `anthropic::tools::execute` calling each other
  defeated rustc's `Send` inference for plain `async fn`s** — a cryptic
  `cannot satisfy `impl Future: Send`` error pointing at an unrelated
  line. Fixed by making `run_turn` return a boxed, type-erased future
  instead of using `async fn` sugar, breaking the cycle at one edge. Not
  something the plan could have flagged, since it's a consequence of
  Rust's type system rather than a design decision — worth remembering
  for the *next* plan against this idea (sandboxing), since a real tool
  proxy will have the same call-each-other shape.
- **The process-global `db::get()` pool doesn't survive being used across
  multiple `#[tokio::test]`s.** The plan's literal `run_turn(id, ...)`
  signature (no explicit pool) implied reaching for `db::get()`
  internally, matching `send_message`'s existing pattern — but each
  `#[tokio::test]` gets its own tokio runtime, and a `sqlx::PgPool`'s
  connections become unusable once the runtime that created them tears
  down. Tests reaching for the shared pool reliably `PoolTimedOut` when
  run concurrently. Fixed by threading `pool: &PgPool` through `run_turn`
  and `execute` explicitly (mirroring `db.rs`'s own established
  convention), letting tests use isolated `#[sqlx::test]` pools
  end-to-end, including through `run_async`'s spawned background task.
  Documented in `testing.md` so the next person doesn't rediscover this
  the hard way.
- **A genuine deadlock**, found only by live-testing `cancel_task`
  against the real model: `cancel_task` runs synchronously inside the
  *calling* `run_turn`'s own tool-dispatch loop (same as any other tool),
  which already holds the per-conversation lock for its entire duration —
  but `cancel_task` also pushes a cancellation notification via an
  *awaited* `chat::run_turn` call, which tried to re-acquire that same
  non-reentrant lock and hung forever. No test caught this until a live
  `curl` request against `dx serve` sat for minutes. Fixed by detaching
  that one push with `tokio::spawn` instead of awaiting it inline, and
  added a regression test (`tokio::time::timeout`-wrapped, reproduces the
  exact scenario) that reliably fails-by-hanging on the old code and
  passes on the fix. **Every other push path is fine** — `run_async`'s
  own completion push happens from its independently-spawned task, not
  synchronously inside the caller's lock-holding loop — `cancel_task` was
  the one place a tool's own execution and its notification push shared a
  call stack.
- **Forgot to add the new tools' `ToolDefinition`s to `send_message`'s
  request** — `run_async` and the six management tools were fully
  implemented and unit-tested, but the model had no way to know they
  existed until a live test asked it to use `run_async` and it correctly
  replied it had no such tool. A reminder that "implemented and tested"
  and "wired into the one place the model actually sees it" are different
  checkpoints — worth a specific test asserting `tool_definitions()`
  contains every dispatchable tool name, so this can't silently regress
  again.
- An emergent (not a bug) discovery: with `stream_output: true`, a
  per-line push can itself trigger the model into a multi-turn detour
  (it spontaneously called `task_result`/`wait_task`/`task_status` after
  seeing a `<task-output>` notification) — and since `count`'s own loop
  `.await`s that push before continuing, the wrapped tool's *own*
  completion is paced by however long that nested exchange takes. This
  is a direct, correct consequence of the plan's explicit ordering
  requirement ("keeps per-task notifications strictly ordered"), not
  something to fix, but worth flagging for whoever designs
  `stream_output` coalescing later — it compounds the plan's
  already-flagged per-line cost concern.
- No browser automation available in this sandbox (no Chrome extension,
  no Playwright/node) — frontend verification leaned on unit tests for
  pure logic (`merge_messages_by_id`, `render_block`, ...) plus
  `curl`-driven live testing of the underlying endpoints and a check that
  the SSR shell + WASM bundle both serve with `200`. The actual rendered
  DOM (task panel layout, live reconnect behavior in a real tab) was
  never visually confirmed. **Resolved in the UI-polish pass** — see next
  bullet.
- **The UI-polish pass got real browser automation, but it took a
  live-in-the-loop round trip to get there**, not something available
  from the start: no root, no npm/pip, and no browser binary existed in
  this container, so a genuinely blocked point was reached and the user
  ran two `apt-get`-requiring commands from outside the session (system
  Chromium deps, then a Python venv + `pip install playwright`) before
  the browser download itself could run unprivileged. Once available,
  it caught the real thing unit tests structurally can't: e.g. confirming
  *both* directions of the sticky-auto-scroll behavior (stays put when
  scrolled up, follows when at the bottom) against actual `scrollTop`/
  `scrollHeight` measurements on the live DOM, not just that the code
  compiles. Baking the install into the `Dockerfile` means this shouldn't
  need to be rebuilt or asked for again.
- **Background-task execution was observed to be unreasonably slow, and
  once outright stuck, live in this sandbox** — a `count` task run via
  `run_async` sat unchanged at `count: 1/8` for several minutes (confirmed
  via direct `curl` to `/api/conversations/{id}/tasks`, not a UI staleness
  artifact), and a separate live model call never produced a reply after
  5+ minutes despite the same request pattern working (if slowly) earlier
  in the same session. Not investigated further at the time — out of
  scope for a UI pass — but flagged here since it happened twice and
  wasn't explained. **Root-caused and fixed as part of applying this
  retro's own proposals** — see "What to change" below.

**What to change (proposals):**
- ~~Add a test asserting `tool_definitions()`'s name list matches every
  branch `anthropic::tools::execute` dispatches on~~ — checked:
  `api::chat::tests::test_tool_definitions_covers_every_dispatchable_tool_name`
  already does exactly this (written during the original implementation
  pass, before this retro entry was drafted) and its hardcoded dispatch
  list still matches `execute`'s match arms 1:1, verified by hand against
  the current code. No change needed.
- ~~Consider documenting the "boxed future breaks a `Send`-inference
  cycle" pattern somewhere more prominent than a doc-comment on
  `run_turn`~~ — done: added as a Rule in `development-process.md`,
  alongside "Bound the boundaries" (the other async-correctness gotcha
  this same project ran into) — see that file rather than duplicating the
  explanation here.
- ~~Re-propose (again) getting real browser automation available in this
  sandbox~~ — done: Playwright is now baked into the dev container image
  (see the UI-polish section above and `testing.md`).
- ~~Investigate the slow/stuck `run_async` task and the one outright-hung
  model call observed live during the UI-polish pass~~ — root cause
  found: `anthropic::stream::stream_anthropic_message`'s initial request
  to Anthropic (`reqwest::Client::new()...send()`, before any streaming
  even starts) had **no timeout at all** — a direct violation of this
  project's own "Bound the boundaries" rule, missed because `CHUNK_TIMEOUT`
  (bounding the gap *between* chunks once streaming has started) reads a
  lot like full coverage but doesn't bound the initial connect-and-headers
  wait. A stalled/hung connect there is indistinguishable from the caller
  hanging forever — and since a `run_async` task with `stream_output: true`
  `.await`s exactly this call once per line before its own loop can
  continue, that's what made the wrapped tool look permanently "stuck"
  rather than surfacing as a visible error. Fixed by adding a
  `RESPONSE_TIMEOUT` (90s) around the initial request, factored into a
  `send_and_await_response` helper so a test (mock TCP listener that
  accepts a connection and never writes a response) can exercise the
  timeout firing without waiting 90 real seconds. Whether the *underlying*
  slowness was sandbox throttling or a real upstream hiccup is still
  unknown — but it can no longer hang the app indefinitely either way,
  which was the actual risk worth closing.
