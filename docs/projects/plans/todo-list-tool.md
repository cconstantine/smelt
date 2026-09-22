# A model-visible todo list tool (todowrite/todoread)

**Branch:** `todo-list-tool`

## What

Two new native tools, `todowrite` and `todoread`, that let the model track
its own multi-step plan as structured state instead of only in plain text —
same idea as opencode's `todowrite`/`todoread`. A small always-visible panel
in the chat UI (same shape/placement as the existing background-task and
sandbox panels) shows the current list live, so the user watches the plan
update as the model works instead of only seeing a final message. See
`docs/projects/ideas/todo-list-tool.md` for the full motivation.

## Design decisions (resolving the idea doc's open questions)

- **Whole-list replace, no per-item ids or partial updates.** `todowrite`
  always sends the complete list; the server stores exactly what it's
  given, overwriting whatever was there. This matches opencode's own
  `todowrite` and sidesteps partial-update races entirely — there's no
  "item 3" to reference between calls, so there's nothing to get out of
  sync. An item's identity for one call is just its position in that
  call's array.
- **No automatic per-turn re-injection into context.** The model sees its
  own prior `todowrite`/`todoread` calls the same way it sees any other
  tool call: they're persisted `ContentBlock`s in ordinary message history,
  replayed on every turn like everything else — no new mechanism needed for
  the common case. This mirrors how live sandbox pod/terminal state already
  works (no per-turn re-injection there either). The one place state *can*
  otherwise fall out of view is across a compaction boundary, and that
  already has a precedent: `describe_live_state` (auto-compaction's
  summarization-prompt builder) explicitly lists every live
  pod/terminal/background-task id so the summary doesn't drop them. This
  plan extends `describe_live_state` to include the current todo list the
  same way, rather than inventing a second mechanism.

## Which files

- **New migration** `migrations/<ts>_create_conversation_todos.sql`: a
  `conversation_todos` table, one row per conversation
  (`conversation_id BIGINT PRIMARY KEY REFERENCES conversations(id) ON
  DELETE CASCADE`), an `items JSONB NOT NULL DEFAULT '[]'` column holding
  the whole list, `updated_at`. Mirrors `conversation_context_usage`'s
  shape exactly (same "last-known-only, not a history" reasoning: a todo
  list is current-state, not an append log).
- **`src/anthropic/tools.rs`**:
  - `TodoItem { content: String, status: TodoStatus }` and
    `TodoStatus` (`Pending`/`InProgress`/`Completed`, `#[serde(rename_all =
    "snake_case")]`) — defined at the top level (outside the
    `#[cfg(feature = "server")]` module), same reasoning as `TaskSummary`:
    the frontend panel needs this type in the `web` build too.
  - `todowrite_tool(pool, conversation_id, input)`: validates/parses
    `input.todos` into `Vec<TodoItem>`, calls
    `db::set_conversation_todos`, publishes
    `events::ConversationEvent::TodoListUpdate`, returns the same
    JSON-serialized list back (confirms what was stored, same pattern
    `list_tasks` uses).
  - `todoread_tool(pool, conversation_id)`: calls
    `db::get_conversation_todos`, returns it JSON-serialized.
  - Both registered in `execute`'s dispatch match and in
    `native_tool_definitions()` (schema: `todowrite` takes `{todos: [{content:
    string, status: enum[pending,in_progress,completed]}]}`; `todoread`
    takes no arguments).
- **`src/db.rs`**: `get_conversation_todos`/`set_conversation_todos`,
  mechanical mirrors of `get_conversation_usage`/`upsert_conversation_usage`
  (flagging this per `development-process.md`'s mechanical-mirror
  exception) — still get a real round-trip characterization test, not just
  a compiles-check.
- **`src/events.rs`**: new `ConversationEvent::TodoListUpdate { items:
  Vec<TodoItem> }` variant — ephemeral UI telemetry, same category as
  `TaskUpdate`, regenerable at any time from `db::get_conversation_todos`.
- **`src/api/chat.rs`**:
  - `get_todos(id) -> ServerFnResult<Vec<TodoItem>>` — one-shot pull for
    initial panel load/reconnect, same shape as `get_tasks`.
  - `describe_live_state` gains the current todo list (if non-empty) in
    its summarization-prompt text, alongside the existing pods/terminals/
    tasks — this is the logic-bearing addition that needs a test (extending
    the existing `test_describe_live_state_...` coverage).
- **`src/frontend/pages/chat.rs`**: a `TodoPanel` component/section
  alongside the existing task/sandbox panels — a `Signal<Vec<TodoItem>>`
  loaded via `get_todos` on conversation select, replaced wholesale (not
  merged — full-replace semantics mean the live event already carries the
  complete list, unlike `TaskUpdate`'s incremental lines) on
  `ConversationEvent::TodoListUpdate`. Each item renders with a status
  marker (open box / in-progress marker / checked box) — plain CSS, no
  charting involved.
- **`assets/chat.css`**: `.todo-panel-*` styling for the list and its
  per-item status markers.
- **`src/browser_tests.rs`**: one more scenario in the existing end-to-end
  tier, seeded directly via `db::set_conversation_todos` (no real model
  call), asserting the panel renders the seeded list and updates live on a
  second write — same bypass-the-model pattern every existing scenario
  uses.
- **Docs**: `docs/api.md` (new server function + tools), `docs/models.md`
  or `docs/architecture.md` (wherever `TaskSummary`'s shared-type note
  lives — same treatment for `TodoItem`), `docs/frontend.md` (new panel),
  `docs/projects/state.md` (new feature bullet once shipped).

## Open questions / tradeoffs

- Status marker glyphs/copy for the panel (plain checkbox-style vs. an
  icon font) — no charting/dataviz involved (this isn't proportional data,
  just a small state list), so this is a plain CSS call made during
  implementation, not something that needs a design pass up front.
- Whether `todowrite` should reject an empty `content` string per item —
  leaning yes (a blank todo is never useful), to be decided as a small
  validation behavior during TDD rather than blocking the plan on it.
  Review: Reject empty 'content' strings.
