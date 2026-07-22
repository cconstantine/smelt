# Database

SQLite via sqlx, matching the pattern used elsewhere in this style of app: a process-wide `OnceLock<SqlitePool>` rather than threading a pool through function arguments or extractors.

```rust
// src/db.rs
static POOL: OnceLock<SqlitePool> = OnceLock::new();

pub async fn init() -> &'static SqlitePool { /* opens the pool, sets POOL */ }
pub fn get() -> &'static SqlitePool { /* panics if init() hasn't run yet */ }
```

`main.rs` calls `db::init()` once at startup, before serving, then runs `sqlx::migrate!()` against it. Every other function — CRUD helpers in `db.rs`, server functions in `api/chat.rs` — just calls `db::get()`.

## Query pattern

`INSERT ... RETURNING *` mapped onto the model struct via `sqlx::query_as::<_, T>`, so a create returns the exact row (including DB-assigned `id`/timestamps) in one round trip:

```rust
pub async fn create_conversation() -> Result<Conversation, sqlx::Error> {
    sqlx::query_as::<_, Conversation>(
        "INSERT INTO conversations (title) VALUES (?) RETURNING *",
    )
    .bind(DEFAULT_TITLE)
    .fetch_one(get())
    .await
}
```

`Conversation`/`Message` derive `sqlx::FromRow` (gated `#[cfg_attr(feature = "server", derive(sqlx::FromRow))]` in `models.rs`, since the derive itself pulls in sqlx types not present on the web build).

## Errors

`db.rs` functions return `Result<T, sqlx::Error>` directly. Server functions convert with `.map_err(ServerFnError::new)` at the boundary — there's no separate hand-rolled `ApiError` type, since `ServerFnError` (from `dioxus::prelude`) already carries a message through to the client and renders sensibly via `{err}` in the UI. If a specific sqlx error needs distinct client-facing handling later (e.g. a unique-constraint violation mapped to a specific message), match on `sqlx::Error` inside the server function before converting — there's no reason to do that generically today, since nothing in the current schema has a uniqueness constraint beyond the primary keys.

## Testing against the database

See [testing.md](testing.md#database-tests) — `db::test_support::init_test_db()` gives every test a shared, migrated, in-memory database.
