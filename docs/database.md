# Database

Postgres via sqlx. `db.rs`'s CRUD functions take `pool: &PgPool` as an explicit parameter rather than reaching into a global — that's what lets `#[sqlx::test]` (see [testing.md](testing.md#database-tests)) hand each test its own isolated database.

A process-wide `OnceLock<PgPool>` still exists for *production* wiring — `main.rs` and `api/chat.rs`'s server functions only open the pool once, at startup/per-request, same as before:

```rust
// src/db.rs
static POOL: OnceLock<PgPool> = OnceLock::new();

pub async fn init() -> &'static PgPool { /* connects (with retry), sets POOL */ }
pub fn get() -> &'static PgPool { /* panics if init() hasn't run yet */ }
```

`main.rs` calls `db::init()` once at startup, before serving, then runs `sqlx::migrate!()` against it. Server functions in `api/chat.rs` call the `db.rs` CRUD functions directly, passing `db::get()` as the pool argument.

## Query pattern

`INSERT ... RETURNING *` mapped onto the model struct via `sqlx::query_as::<_, T>`, so a create returns the exact row (including DB-assigned `id`/timestamps) in one round trip. Placeholders are Postgres-style `$1, $2, ...`:

```rust
pub async fn create_conversation(pool: &PgPool) -> Result<Conversation, sqlx::Error> {
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (title) VALUES ($1) RETURNING *",
    )
    .bind(DEFAULT_TITLE)
    .fetch_one(pool)
    .await
}
```

A plain `DELETE` needs no `RETURNING`/`FromRow` mapping at all:

```rust
pub async fn delete_conversation(pool: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
```

Deleting an id that doesn't exist is not an error — it just affects zero
rows. `messages.conversation_id` has `ON DELETE CASCADE`, which Postgres
enforces natively (no pragma needed, unlike SQLite), so this cleans up the
conversation's messages for free.

`Conversation`/`Message` derive `sqlx::FromRow` (gated `#[cfg_attr(feature = "server", derive(sqlx::FromRow))]` in `models.rs`, since the derive itself pulls in sqlx types not present on the web build).

## Errors

`db.rs` functions return `Result<T, sqlx::Error>` directly. Server functions convert with `.map_err(ServerFnError::new)` at the boundary — there's no separate hand-rolled `ApiError` type, since `ServerFnError` (from `dioxus::prelude`) already carries a message through to the client and renders sensibly via `{err}` in the UI. A few tables (`mcp_servers.name`, `sandbox_volumes.name`) do have a `UNIQUE` constraint beyond their primary key, and a violation surfaces exactly this way today — the raw Postgres error message (`duplicate key value violates unique constraint "..."`) reaches the browser as-is, not mapped to anything friendlier. If a specific sqlx error needs distinct client-facing handling later, match on `sqlx::Error` inside the server function before converting.

## Testing against the database

See [testing.md](testing.md#database-tests) — `#[sqlx::test]` gives every test its own isolated, migrated Postgres database, passed in as a `PgPool` argument.
