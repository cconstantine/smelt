# Database

Postgres via sqlx. `db.rs`'s CRUD functions take `pool: &PgPool` as an explicit parameter rather than reaching into a global — that's what lets `#[sqlx::test]` (see [testing.md](testing.md#database-tests)) hand each test its own isolated database.

A process-wide `OnceLock<PgPool>` still exists for *production* wiring, so the pool is opened once, at startup:

```rust
// src/db/mod.rs
static POOL: OnceLock<PgPool> = OnceLock::new();

pub async fn init() -> &'static PgPool { /* connects (with retry), sets POOL */ }
pub fn get() -> &'static PgPool { /* panics if init() hasn't run yet */ }
```

`main.rs` calls `db::init()` once at startup, before serving, then runs `sqlx::migrate!()` against it. `init()` panics if `DATABASE_URL` isn't set or doesn't parse, opens a pool of at most 5 connections, and retries a failed connect up to 10 times, 500 ms apart (Postgres may still be starting), before panicking.

Server functions in `src/api/` (`chat.rs`, `git.rs`, `language_servers.rs`, `mcp.rs`, `pods.rs`, `sandbox_volumes.rs`) call the `db.rs` functions directly, passing `db::get()` as the pool argument. Server code outside `api/` (`sandbox/`, `git.rs`, `mcp_oauth.rs`, `anthropic/tools.rs`, `lsp/`, ...) doesn't reach for `db::get()` itself: it takes a `&PgPool` parameter, passed down from a server function or from `main.rs`.

## Query pattern

`INSERT ... RETURNING *` mapped onto the model struct via `sqlx::query_as::<_, T>`, so a create returns the exact row (including DB-assigned `id`/timestamps) in one round trip. Placeholders are Postgres-style `$1, $2, ...`:

```rust
pub async fn create_conversation(pool: &PgPool) -> Result<Conversation, sqlx::Error> {
    sqlx::query_as::<_, Conversation>("INSERT INTO conversations (title) VALUES ($1) RETURNING *")
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

Deleting an id that doesn't exist is not an error — it just affects zero rows. Deleting a conversation cascades through everything hanging off it (see [Deleting a conversation](database/schema.md#deleting-a-conversation)).

Other patterns in `db.rs`:

- **Upserts** use `INSERT ... ON CONFLICT`: `DO UPDATE` for last-known-only rows (`conversation_context_usage`, `conversation_todos`, `git_settings`, `repo_trust`, `loaded_instructions`, `instruction_requests`), `DO NOTHING` where an existing row wins (`sandbox_pod_previews`, `ensure_mcp_server`).
- **Single values** (an `EXISTS`, one column) use `sqlx::query_scalar`.
- **JSONB columns** go through `sqlx::types::Json<T>`: bound as `.bind(sqlx::types::Json(value))`, read back as a `Json<T>` field or scalar and unwrapped (`.0`).
- **Soft delete**: `sandbox_pods` and `sandbox_terminals` are never deleted by smelt; terminating one sets `terminated_at = now()`, so the command history under it survives. "Live" means `terminated_at IS NULL`.
- **Private row struct**: `language_servers` is read through a private `LanguageServerRow` (JSONB fields as `Json<T>`) converted into `models::LanguageServer` with `From`. Its queries name their columns via the `LANGUAGE_SERVER_COLUMNS` constant rather than `RETURNING *`/`SELECT *`, so the column list matches the struct.

## FromRow

`Conversation`/`Message` in `models.rs`, and `anthropic::TokenUsage`, derive `sqlx::FromRow` gated as `#[cfg_attr(feature = "server", derive(sqlx::FromRow))]`, since they're compiled into the web build too and the derive pulls in sqlx types not present there. The row structs defined in `db.rs` itself (`SandboxPod`, `SandboxVolume`, `McpServerConfig`, `ConversationRepo`, ...) derive it ungated: the `db` module is only compiled with the `server` feature. `models::LanguageServer` has no `FromRow`; see the private row struct above.

## Errors

`db.rs` functions return `Result<T, sqlx::Error>` directly. Server functions convert with `.map_err(ServerFnError::new)` at the boundary — there's no separate hand-rolled `ApiError` type, since `ServerFnError` (from `dioxus::prelude`) already carries a message through to the client and renders sensibly via `{err}` in the UI. If a specific sqlx error needs distinct client-facing handling, match on it before converting (`e.as_database_error().is_some_and(|d| d.is_unique_violation())`).

Three unique violations are mapped that way today:

- `language_servers.name`: `lsp/config.rs` returns "A language server named … already exists."
- `ssh_keys_only_one`: `git.rs` returns the "There's already an SSH key" refusal when another request added a key meanwhile.
- `sandbox_pods_one_live_per_conversation`: `sandbox` returns `SandboxError::PodAlreadyExists`.

Any other violation (e.g. a duplicate `mcp_servers.name` or `sandbox_volumes.name`) reaches the browser as the raw Postgres message (`duplicate key value violates unique constraint "..."`).

## Migrations

`migrations/*.sql`, applied in filename order by `sqlx::migrate!()` at startup. Never edit a migration once it has been applied anywhere: sqlx stores each applied migration's checksum and refuses to start against a database whose recorded checksum no longer matches the file. The schema changes by adding a new migration.

An upgrade from before SME-439 gets only added columns (`20261106180000_learned_context_window.sql`), but the old learners wrote `context_window` itself, and their numbers are indistinguishable from values the user set (the 160,799 of the incident): clear any Context window value you don't recognize after upgrading (on the host, the incident row was hand-fixed before the upgrade).

## Schema

Every table, and what deleting a conversation removes: [database/schema.md](database/schema.md).

## Testing against the database

See [testing.md](testing.md#database-tests) — `#[sqlx::test]` gives every test its own isolated, migrated Postgres database, passed in as a `PgPool` argument.
