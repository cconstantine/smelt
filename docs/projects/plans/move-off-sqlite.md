# Move off SQLite to Postgres

**Branch:** `move-off-sqlite` · **Idea:** [`projects/ideas/move-off-sqlite.md`](../ideas/move-off-sqlite.md)

## What

Replace SQLite with Postgres as smelt's persistence layer for the schema
as it exists today (`conversations`, `messages`, plain-text `content`).
This lands *before* the `tool-use-round-trip` plan's implementation, so
that plan — and the schema churn after it — builds on Postgres rather
than adding more tables/columns to SQLite first, per the idea doc's
sequencing question.

`data/smelt.db` currently has zero rows in both tables (checked directly)
— there is no real conversation history to carry forward, so this is a
clean cutover, not a data migration.

## Why

Per the idea doc: concurrent tool-loop writes and the schema churn coming
from the coding-session work are cheaper to accommodate now, while the
schema is still small and there's no real data, than after several more
schema generations land on SQLite first.

## Which files

- **`Cargo.toml`** — swap sqlx's `sqlite` feature for `postgres`. Chosen
  test approach (`#[sqlx::test]`, see Open questions) likely needs an
  additional sqlx feature to be enabled — confirm the exact name against
  the sqlx source in `~/.cargo/registry/src` before adding it, not from
  memory (existing rule in `development-process.md`).
- **`docker-compose.yml`** — add a `postgres` service (official `postgres`
  image) with a named volume for its data directory (matching the
  existing `bash-history` volume pattern) so local data survives container
  recreation. `smelt`'s service gets `depends_on: postgres`. Dev-only
  credentials default via compose variable substitution, matching the
  existing `UID`/`GID` pattern (e.g. `${POSTGRES_PASSWORD:-smelt}`) rather
  than requiring a secret up front — this is a local single-user dev
  database, not a shared/production one. No `ports:` mapping — `postgres`
  is reachable only from other compose services (`smelt`, by service
  name) on the compose network, not from the host.
- **`migrations/20260721000000_create_conversations.sql`,
  `migrations/20260721000001_create_messages.sql`** — rewritten in place
  to Postgres dialect (see How). This is a deliberate, one-time exception
  to `migrations.md`'s "never edit an applied migration" rule: these files
  were only ever applied to the SQLite database being abandoned here, no
  Postgres database has run them, so there's no live checksum to protect.
  Worth saying explicitly in the commit/PR rather than silently doing
  something the docs otherwise forbid.
- **`src/db.rs`** — the bulk of the change:
  - `SqlitePool`/`SqliteConnectOptions`/`SqlitePoolOptions` → `PgPool`/
    `PgConnectOptions`/`PgPoolOptions`.
  - `init()` drops the `create_if_missing` + parent-directory-creation
    logic entirely (Postgres needs the target database to already exist —
    the official image provisions one from `POSTGRES_DB` on first boot)
    and the `journal_mode`/`foreign_keys` pragmas (no SQLite-style pragma
    equivalent; Postgres always enforces FKs). Adds a small bounded retry
    loop around the initial connection — "container started" doesn't mean
    "Postgres is accepting connections yet," especially on first boot
    while it initializes its data directory, and today's single
    `.connect_with(...).await.expect(...)` would flake on that.
  - Every CRUD function (`create_conversation`, `list_conversations`,
    `list_messages`, `create_message`, `delete_conversation`) changes from
    calling `get()` internally to taking `pool: &PgPool` as an explicit
    parameter. This is the one real architectural deviation from what
    `database.md`/`architecture.md` currently document ("a process-wide
    `OnceLock` rather than threading a pool through function arguments")
    — forced by the test approach (see How) rather than a stylistic
    preference. The global `OnceLock<PgPool>` stays for *production*
    wiring (`db::init()`/`db::get()` still exist; `main.rs` and
    `api/chat.rs`'s server functions still only reach for the pool once,
    at the top of each request), it's just no longer reached into from
    inside `db.rs` itself.
  - `?` positional placeholders → Postgres's `$1, $2, ...`; `datetime('now')`
    → `now()`; `substr(?, 1, 60)` → `left(?, 60)`.
  - `test_support::init_test_db()` and its shared-cache-in-memory setup
    are deleted outright — no equivalent exists for Postgres and none is
    needed once tests use `#[sqlx::test]`.
- **`src/api/chat.rs`** — every call site (`db::list_conversations()`,
  `db::create_message(id, "user", &content)`, etc.) passes `db::get()`
  explicitly now that the callee takes a pool parameter. No behavior
  change, purely mechanical.
- **`.env.example`, `docs/setup.md`** — `DATABASE_URL` default changes
  from `sqlite:./data/smelt.db` to a Postgres connection string matching
  whatever the compose service is configured with.
- **`docs/database.md`** — rewrite the whole "process-wide `OnceLock`"
  framing to describe the pool-as-parameter pattern for `db.rs` functions,
  with the global pool as a production-wiring detail rather than the
  headline pattern. Update the query-pattern examples' placeholder syntax.
- **`docs/migrations.md`, `docs/testing.md`, `docs/architecture.md`** —
  SQLite-specific notes (pragmas, the in-memory shared-cache trick, journal
  mode) replaced with their Postgres equivalents (or removed where there
  isn't one).
- **Tests** (`src/db.rs`, `src/anthropic/stream.rs` untouched, `src/api/chat.rs`
  if it grows DB-touching tests later) — every existing `#[tokio::test]`
  that called `init_test_db()` then `db::get()`-backed functions converts
  to `#[sqlx::test] async fn test_x(pool: PgPool)`, calling the (now
  pool-taking) db functions directly with that pool. One test per test
  function gets its own isolated database automatically — no shared
  fixture, no manual setup/teardown.

## How

**Dialect rewrite, not a redesign.** The schema itself doesn't change —
same two tables, same columns, same `CHECK`/`ON DELETE CASCADE`
constraints (Postgres supports both natively). This is a straight
port: `INTEGER PRIMARY KEY AUTOINCREMENT` → `BIGINT GENERATED ALWAYS AS
IDENTITY PRIMARY KEY` (keeps `id: i64` in `models.rs` unchanged — plain
`SERIAL` would be `i32` and doesn't fit), `DATETIME` → `TIMESTAMP`.

**No compile-time query checking today, so no new build-time DB
dependency.** `db.rs` uses `sqlx::query`/`query_as` (runtime-checked), not
the `sqlx::query!` macro — confirmed by reading the current file. That
means `cargo build`/`cargo check` won't need a reachable `DATABASE_URL` at
compile time after this change, which is worth confirming stays true
rather than something a later plan accidentally breaks by reaching for
`sqlx::query!` for its compile-time safety.

**Why the pool has to become a parameter.** `#[sqlx::test]` creates a
fresh, isolated Postgres database per test and hands it to the test
function as an argument — it has no mechanism to populate a process-global
`OnceLock` (and couldn't: `OnceLock::set` only succeeds once per process,
but every test needs its *own* pool). Threading `&PgPool` through `db.rs`'s
functions is the standard, idiomatic sqlx shape and is what makes
`#[sqlx::test]` usable at all here; keeping `db::get()` for production
call sites means `api/chat.rs` and `main.rs` don't get meaningfully more
verbose — one `db::get()` per request, same as today, just passed
explicitly instead of reached for implicitly three functions deep.

## Open questions / tradeoffs

- **`cargo test` now requires Postgres to be running first** (via
  `docker compose up -d`) — a real workflow change from today's fully
  self-contained SQLite in-memory tests. This was the explicit tradeoff
  accepted when choosing `#[sqlx::test]` over `testcontainers-rs` (which
  would have kept `cargo test` self-contained at the cost of a new
  dependency and unconfirmed Docker-socket access from the test
  environment) — flagged here so it's a known, accepted cost rather than
  a surprise.
- **Startup connection retry bound** — proposing a handful of attempts
  with a short backoff (exact numbers TBD at implementation time) rather
  than either a single unretried attempt (today's behavior, flaky on
  first boot) or an unbounded retry loop (would hang forever if Postgres
  is genuinely misconfigured, hiding a real error).
