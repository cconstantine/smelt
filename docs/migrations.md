# Migrations

`migrations/*.sql`, named `YYYYMMDDHHMMSS_description.sql` — sqlx's migrator orders and applies them by filename, so the timestamp prefix must sort correctly and never repeat. Run automatically by `main.rs` on startup (`sqlx::migrate!().run(pool).await`) and by `db::test_support::init_test_db()` against the shared in-memory test database.

Current migrations:

| File | Adds |
|---|---|
| `20260721000000_create_conversations.sql` | `conversations` table |
| `20260721000001_create_messages.sql` | `messages` table + `idx_messages_conversation_id` |

## SQLite notes

- `role TEXT NOT NULL CHECK (role IN ('user', 'assistant'))` enforces the message role at the database level rather than trusting callers.
- `ON DELETE CASCADE` on `messages.conversation_id` means deleting a conversation (not implemented yet, but trivial to add) cleans up its messages for free — requires `PRAGMA foreign_keys = ON`, which `db::init()` already sets per-connection.
- Timestamps default via `DATETIME NOT NULL DEFAULT (datetime('now'))` rather than being set from application code, so `INSERT ... RETURNING *` gets a DB-assigned value consistent across all insert paths.

Writing a new migration: add a new timestamped file, never edit an already-applied one (sqlx checksums applied migrations and will refuse to start if a checksum changes).
