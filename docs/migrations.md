# Migrations

`migrations/*.sql`, named `YYYYMMDDHHMMSS_description.sql` — sqlx's migrator orders and applies them by filename, so the timestamp prefix must sort correctly and never repeat. Run automatically by `main.rs` on startup (`sqlx::migrate!().run(pool).await`) and by `#[sqlx::test]` against each test's freshly created database.

Current migrations:

| File | Adds |
|---|---|
| `20260721000000_create_conversations.sql` | `conversations` table |
| `20260721000001_create_messages.sql` | `messages` table + `idx_messages_conversation_id` |
| `20260727153258_backfill_message_content_as_json_blocks.sql` | rewrites existing `messages.content` from plain text into the single-`Text`-block JSON shape `Message::blocks()` now expects (see [models.md](models.md)) |

## Postgres notes

- `role TEXT NOT NULL CHECK (role IN ('user', 'assistant'))` enforces the message role at the database level rather than trusting callers.
- `ON DELETE CASCADE` on `messages.conversation_id` means deleting a conversation cleans up its messages for free — Postgres enforces foreign keys natively, no pragma/setting needed.
- Timestamps default via `TIMESTAMP NOT NULL DEFAULT now()` rather than being set from application code, so `INSERT ... RETURNING *` gets a DB-assigned value consistent across all insert paths.
- `id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY` (not `SERIAL`) — keeps `id: i64` in `models.rs`; plain `SERIAL` is `i32` and doesn't fit.

Writing a new migration: add a new timestamped file, never edit an already-applied one (sqlx checksums applied migrations and will refuse to start if a checksum changes). The two migrations above are a deliberate one-time exception — they were rewritten in place from SQLite to Postgres dialect during the move off SQLite, but had only ever been applied to the SQLite database being abandoned in that move, so there was no live Postgres checksum to protect.
