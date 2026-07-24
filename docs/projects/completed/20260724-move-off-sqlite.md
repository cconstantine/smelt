# Move off SQLite to Postgres

**Branch:** `move-off-sqlite` · **Idea:** `projects/ideas/move-off-sqlite.md` (removed) · **Plan:** `projects/plans/move-off-sqlite.md` (removed)

## What shipped

Smelt's persistence layer moved from SQLite to Postgres — same schema
(`conversations`, `messages`), no data migration needed (the SQLite database
had zero rows in both tables at cutover time).

- Migrations rewritten in place to Postgres dialect (`BIGINT GENERATED
  ALWAYS AS IDENTITY`, `TIMESTAMP`, `now()`) — a deliberate one-time
  exception to "never edit an applied migration," since they'd only ever
  run against the SQLite database being abandoned here.
- `db.rs`'s CRUD functions now take `pool: &PgPool` explicitly instead of
  reaching into a global `OnceLock` internally. The global pool
  (`db::init()`/`db::get()`) still exists for production wiring; it's just
  no longer reached into from inside `db.rs` itself.
- Tests converted from the old shared in-memory-SQLite fixture to
  `#[sqlx::test]` — each test gets its own freshly created, migrated
  Postgres database.
- `docker-compose.yml` gained a `postgres` service (dev-only, no `ports:`
  mapping, reachable only from `smelt` over the compose network).
  `DATABASE_URL` has no default — `db::init()` panics on startup if it's
  unset — and is set explicitly in the `smelt` service's environment,
  reusing the same `POSTGRES_PASSWORD` compose variable as the `postgres`
  service so the two can't drift.
- Docs (`database.md`, `migrations.md`, `testing.md`, `architecture.md`,
  `setup.md`, `state.md`) updated to match.

## Retrospective

**What worked:**
- The plan doc (written in an earlier session) was thorough enough that
  implementation was close to mechanical — every file it named needed
  exactly the change it described, and the "Open questions" section
  correctly flagged the two things that mattered (`#[sqlx::test]` requiring
  a live Postgres, and the retry-loop bound) with no other surprises.
- Verifying the sqlx `postgres` feature against the actual crate source
  (downloaded via `static.crates.io`, since `crates.io`'s API endpoint
  itself returned a 403 in this sandbox) paid off twice: it confirmed
  `macros` is already default-enabled — no new feature needed — and,
  by reading `sqlx-postgres`'s TLS-upgrade code directly, confirmed that
  `PgSslMode::Prefer` falls back to plaintext when no TLS backend is
  compiled in, so the local dev connection doesn't need a `tls-*` feature
  at all. Both would have been easy to get wrong from memory.
- The pool-as-parameter refactor across `db.rs` and `api/chat.rs` was
  genuinely mechanical (per the plan's own framing) — `cargo check`
  caught every call site that needed `db::get()` threaded through, nothing
  needed manual hunting.
- User feedback landed in two quick, well-scoped rounds after the initial
  cut: first "add `DATABASE_URL` to the compose env" (making the pool's
  fallback default redundant), then "remove the default, fail loudly if
  unset" — a clean example of preferring a hard startup crash over a
  silently-wrong default for required config.

**What caused friction / surprises:**
- No Docker in this sandbox, so the initial implementation pass could only
  be verified with `cargo check` on both targets — `cargo test --features
  server` had to wait until the user confirmed a Postgres instance was
  reachable (a sibling container on the compose network, started outside
  this session). Once reachable, all 15 tests (including all 7
  `#[sqlx::test]` DB tests) passed on the first run.
- `gh` was not installed and there was no `GITHUB_TOKEN` in the
  environment — the same gap the `delete-conversations` retrospective
  flagged and proposed fixing, which apparently wasn't applied before this
  session. Worked around it by downloading the `gh` release tarball
  directly from GitHub and placing the binary in `~/.local/bin` (already
  on `PATH`, no root needed), then having the user run `gh auth login`
  interactively via the `!`-prefixed command since the device-flow login
  needs a real browser. This is now the second time this exact friction
  has come up.

**What to change (proposals — not yet applied):**
- Re-propose (more strongly, since it's now recurred once): bake `gh` into
  the dev container image, or document a supported non-interactive
  auth path, so PR creation doesn't require an ad hoc binary install each
  time a session needs it.
- Consider noting in `docs/setup.md` or a new environment note that this
  sandbox can reach `static.crates.io` (crate tarball downloads) even when
  `crates.io`'s API (`/api/v1/crates/...`) is blocked — useful for the
  "verify external crate APIs against source" rule in
  `development-process.md` next time a local registry cache isn't
  populated.
