# Delete conversations — Plan

**Branch:** `delete-conversations`

Resolves `docs/projects/ideas/delete-conversations.md`. Confirmed with the
user before writing this plan: **hard delete** (no soft-delete/undo), and
an **inline confirm** in the sidebar (no modal).

## What is being built and why

Let the user permanently remove a conversation from the sidebar. There is
currently no way to do this — `docs/api.md` and `docs/projects/state.md`
both flag it as a known gap, and conversations accumulate with no way to
clear them out.

## Files to modify

- `src/db.rs` — new `delete_conversation(id: i64) -> Result<(), sqlx::Error>`, plus tests.
- `src/api/chat.rs` — new `#[delete("/api/conversations/{id}")] pub async fn delete_conversation`.
- `src/frontend/pages/chat.rs` — delete affordance + inline confirm on each sidebar row; handle deleting the currently-open conversation.
- `docs/api.md` — add the new endpoint to the table; drop "delete" from the "not yet implemented" line (keep "rename a conversation, concurrent-send guarding").
- `docs/database.md` — document `delete_conversation`.
- `docs/projects/state.md` — remove "Deleting ... conversations" from the out-of-scope list (leave "renaming" there).

**No new migration.** `messages.conversation_id` already has
`ON DELETE CASCADE` and `db::init()` already sets `PRAGMA foreign_keys =
ON` on every pooled connection (`SqliteConnectOptions::pragma` in the
connect options, so it applies to every connection the pool opens, not
just one) — `docs/migrations.md`'s SQLite notes already called this out as
"trivial to add." Deleting a conversation row cascades to its messages for
free.

## How

### `db::delete_conversation`

```rust
pub async fn delete_conversation(id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(id)
        .execute(get())
        .await?;
    Ok(())
}
```

Deleting an id that doesn't exist affects 0 rows and is **not** an error —
plain SQL DELETE semantics, no "not found" special case needed given
there's a single user and no concurrent multi-client editing to race
against.

### Server function

```rust
#[delete("/api/conversations/{id}")]
pub async fn delete_conversation(id: i64) -> ServerFnResult<()> {
    db::delete_conversation(id).await.map_err(ServerFnError::new)
}
```

Same shape as the other CRUD server functions in the file.

### Frontend (`ConversationSidebar`)

- Each `conversation-item` row gets a delete affordance. Its click handler
  calls `stop_propagation` so clicking delete doesn't also select the row.
- Inline confirm state is a single `pending_delete: Signal<Option<i64>>`
  on `ConversationSidebar` (not per-row state): first click on a row's
  delete button sets `pending_delete` to that id and the row renders a
  "confirm?" affordance in place of the normal delete button; a second
  click while pending actually deletes; clicking a different row's delete
  (or selecting a different conversation) resets `pending_delete` rather
  than stacking multiple rows in a pending state.
- On confirmed delete: call `delete_conversation(id)`; on success, remove
  it from the local `conversations` signal (same optimistic-local-update
  pattern `new_conversation` already uses) and clear `pending_delete`. On
  error, surface it via the existing `error: Signal<Option<String>>` used
  by the rest of `ConversationSidebar`.
- If the deleted conversation is the one currently open
  (`selected() == Some(id)`), reset `selected` to `None`. `ChatPanel`
  already renders its "Select or start a conversation" empty state for
  `None` — no need to auto-select another conversation.

## Tests

Mechanical-mirror of the existing `db.rs` CRUD tests (still asserting real
behavior, per `development-process.md`'s TDD rules — not just "compiles"):

- `test_delete_conversation_removes_it_from_list` — create, delete, assert
  `list_conversations()` no longer contains it.
- `test_delete_conversation_cascades_to_messages` — create a conversation
  with messages, delete it, assert `list_messages(id)` now returns empty
  (proves the FK cascade actually fires, not just that the conversation
  row is gone).
- `test_delete_nonexistent_conversation_is_a_no_op` — call
  `delete_conversation` on an id that was never created, assert `Ok(())`.

No automated frontend/browser test tier exists yet (`state.md`'s
out-of-scope list) — the sidebar delete UX is manually verified per
`docs/testing.md`, same as the rest of the frontend today.

## Open questions / tradeoffs

None outstanding — confirm-UX and hard-vs-soft-delete were the idea doc's
open questions, and both were settled with the user before this plan was
written. The remaining judgment calls (single shared `pending_delete`
signal rather than per-row state, nonexistent-id-is-a-no-op, reset to the
empty state rather than auto-selecting another conversation) are decided
above rather than left open, since none of them need more than a
sentence's justification.
