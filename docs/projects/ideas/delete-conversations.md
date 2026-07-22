# Delete conversations

## What

Let the user delete a conversation from the sidebar. Currently there's no
way to remove one once created — see `docs/api.md`'s "not yet implemented"
note and `state.md`'s out-of-scope list.

## Why

The sidebar (`src/frontend/pages/chat.rs`) only ever grows; a single-user
app with no auth and no archiving means test conversations, dead ends, and
one-off experiments accumulate with no way to clear them out.

## Rough shape

- `db.rs`: `delete_conversation(id: i64) -> Result<(), sqlx::Error>`. Foreign
  key from `messages` to `conversations` needs `ON DELETE CASCADE` (check
  the existing migration) or an explicit `DELETE FROM messages WHERE
  conversation_id = ?` first.
- `api/chat.rs`: `#[delete("/api/conversations/{id}")] pub async fn
  delete_conversation(id: i64) -> ServerFnResult<()>`, mirroring the
  existing CRUD server functions.
- Frontend: a delete affordance per sidebar row in `chat.rs`; needs a
  confirmation step (irreversible, no undo) and must handle deleting the
  conversation that's currently open (redirect to another conversation or
  the empty state).

## Open questions

- Confirm inline (e.g. click-to-confirm) or a modal?
- Hard delete only, or worth soft-deleting (`deleted_at`) given there's no
  multi-user/audit need yet? Hard delete is simpler and matches the
  single-user, no-history-value nature of the app — but flag it for the
  plan phase rather than deciding here.
