# A model-visible todo list tool (todowrite/todoread)

**Branch:** `todo-list-tool` · **Idea:** `projects/ideas/todo-list-tool.md` (removed) · **Plan:** `projects/plans/todo-list-tool.md` (removed)

## What shipped

Two new native tools, `todowrite` and `todoread`, that let the model track
its own multi-step plan as structured state instead of only in plain text —
same idea as opencode's `todowrite`/`todoread`. A live todo panel in the
chat UI (same shape/placement as the existing background-task and sandbox
panels) shows the current list update as the model works, checked/
half-filled/empty status markers per item.

- **`todowrite`** is always a whole-list replace — no per-item ids, no
  partial updates. The server stores exactly what it's given, rejecting a
  blank `content` string per item before persisting anything.
- **`todoread`** reads the current list back.
- Storage mirrors `conversation_context_usage`'s exact shape: one JSONB
  row per conversation (`conversation_todos`), "last-known-only," not a
  history — flagged and implemented as `development-process.md`'s
  mechanical-mirror exception, still with a real round-trip
  characterization test.
- A new `ConversationEvent::TodoListUpdate` carries the complete list on
  every `todowrite` call, so the frontend panel overwrites its signal
  wholesale rather than merging (unlike `TaskUpdate`, which is
  incremental).
- The current todo list survives a compaction boundary via an extension to
  `describe_live_state` (auto-compaction's summarization-prompt builder) —
  the same mechanism already used for live sandbox pod/terminal/task
  state, rather than a new per-turn re-injection mechanism.

**Verification:** 257 `cargo test --features server` tests passing (up
from 250), both build targets clean, the extended automated browser tier
(7 scenarios, up from 6) passing against a real headless Chrome — cold
load from a seeded list, then a live full-replace update via the real
`todowrite` tool path with no reload — and a manual Playwright screenshot
check against a real `dx serve` instance confirming the status markers
render correctly.

## Retrospective

**What worked:**
- **Whole-list-replace as the first design decision, before writing any
  code**, eliminated an entire class of partial-update races (no per-item
  ids to get out of sync) — the same "reason through the shape on paper
  first" discipline `auto-compaction`'s retrospective named, applied here
  to this feature's own scope rather than a wire protocol.
- **Reusing an existing mechanism instead of building a new one for
  "survives compaction."** `describe_live_state` already existed
  specifically to keep live state visible across a compaction boundary
  (pods/terminals/tasks); extending it to include the current todo list
  was a small, low-risk addition instead of inventing a second, parallel
  mechanism for the same problem.
- **Mirroring `conversation_context_usage`'s table shape exactly** (single
  JSONB row per conversation, upsert-on-write) made the DB layer close to
  free — a genuine mechanical mirror, flagged as such, that still got a
  real round-trip test rather than skipping straight to "it compiles."

**What caused friction, surprise, or rework:**
- **A stale WASM bundle produced a real, confusing test failure.** The
  browser tier's server-side Rust code recompiled cleanly with every edit,
  but the browser-side WASM it actually serves is a separately-built
  artifact (`dx build --platform web`) that `cargo test` never rebuilds —
  already documented in `testing.md`, but not front-of-mind at the moment
  the browser test failed with a generic "cold load should show the
  seeded todo list," which looked at first like a real rendering bug in
  the new panel rather than a stale-asset issue. A quick debug print of
  `document.body.innerText` (showing a Dioxus dev-mode "app is being
  rebuilt" placeholder, not the actual page) was what pointed at the real
  cause.
- **This project's own close-out was skipped.** After the PR merged, work
  moved straight to scoping the next idea (`webfetch`) without doing the
  retrospective/completed-doc/`state.md`-update/idea-plan-removal steps
  `development-process.md` calls for — caught only because the user
  noticed the stale plan file still sitting in `projects/plans/` and asked
  about it, not by any self-check.

**What to change:**
- **Proposing** (not yet applied — needs confirmation): add an explicit
  checklist item near the top of `development-process.md`'s Phase 1
  ("Plan") — before starting a new idea, confirm the *previous* project
  was actually closed out (retrospective, completed doc, `state.md`
  update, idea/plan files removed), not just merged. Right now the
  close-out steps are documented (under "Keeping project docs current")
  but nothing prompts checking they were done before moving to the next
  thing, which is exactly how this one slipped.
- **Proposing** (not yet applied — needs confirmation): `development-process.md`'s
  "Definition of done" section names the browser-tier test command
  directly but doesn't mention `dx build --platform web` as a
  prerequisite inline — that requirement only lives in `testing.md`'s own
  browser-check section. A one-line cross-reference right next to the
  existing browser-tier command would surface it at the point it's
  actually needed, instead of relying on having read that section of
  `testing.md` recently enough to remember it.
