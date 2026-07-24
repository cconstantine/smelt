# Testing

## Structure

Tests are inline `#[cfg(test)]` modules in the same file as the code they cover — no separate `tests/*.rs` unit-test tree.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_thing() { assert_eq!(1 + 1, 2); }

    #[tokio::test]
    async fn test_async_thing() { /* ... */ }
}
```

## Database tests

`db.rs`'s CRUD functions take `pool: &PgPool` as an explicit parameter (see [database.md](database.md)), so tests use `#[sqlx::test]` instead of `db::get()`'s process-wide pool — each test function gets its own freshly created, migrated Postgres database, handed in as an argument:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn test_thing_round_trip(pool: PgPool) {
        let c = create_conversation(&pool).await.expect("create");
        // ... exercise db functions against `c.id`, passing `&pool` explicitly
    }
}
```

`#[sqlx::test]` connects to the Postgres server at `DATABASE_URL`, creates a new database per test, runs all migrations against it, and tears it down afterward — no shared fixture, no manual setup/teardown, and no cross-test interference since each test is fully isolated. This requires a reachable Postgres server while running tests (`docker compose up -d postgres`) — a real workflow change from the old in-memory-SQLite setup, where `cargo test` was fully self-contained.

## Testing the Anthropic streaming client without the network

`anthropic::stream::stream_anthropic_message` is tested against a mock upstream — a throwaway Axum server bound to an ephemeral port, with `ANTHROPIC_BASE_URL` pointed at it for the duration of the test:

```rust
#[tokio::test]
async fn test_stream_anthropic_message_assembles_deltas_from_mock_upstream() {
    // spawn a tiny axum::Router that responds to POST /v1/messages with a
    // hand-written text/event-stream body
    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", format!("http://{addr}")) };
    let assembled = stream_anthropic_message("test-key", &request, |delta| { /* collect */ }).await?;
    assert_eq!(assembled, "Hello!");
}
```

`ANTHROPIC_BASE_URL` is process-global, so only one test sets it — parallel test threads can't observe a torn value. The pure parsing logic (`interpret_stream_event`, deciding what a single decoded SSE payload means) is tested separately and synchronously, with no network or async runtime involved at all.

## Running tests

```bash
cargo test --features server                 # the real (server-gated) tests
cargo test --features server -- --nocapture   # show println! output
cargo test --features server test_name        # a single test by name
```

Most logic lives behind the `server` feature; plain `cargo test` compiles but skips it. See [Definition of done](development-process.md#definition-of-done) for the full two-target check.

## What's not covered yet

- **No automated browser tier.** UI/interaction changes are verified manually — `dx serve --fullstack` plus a real or scripted browser (a headless Chrome session driven over the DevTools Protocol worked well for a one-off pass: navigate, click, type, submit, read back rendered DOM state and console errors, screenshot). Worth formalizing into an automated test crate (mirroring the shape of a `fantoccini`-based E2E tier) once there's enough UI surface to justify the setup cost.
- **No native SSR component-test harness.** Components aren't unit-tested by rendering them to a string outside a real page load. Worth adding if/when component logic grows complex enough that manual browser verification alone becomes slow to iterate on.
