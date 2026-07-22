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

`db::get()` reads a process-wide `OnceLock` pool, so DB code needs a shared fixture rather than a fresh database per test:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use test_support::init_test_db;

    #[tokio::test]
    async fn test_thing_round_trip() {
        init_test_db().await; // idempotent; first caller migrates an in-memory DB
        let c = create_conversation().await.expect("create");
        // ... exercise db functions against `c.id`
    }
}
```

`init_test_db` (in `src/db.rs`) opens a **named, shared-cache** in-memory SQLite database (`file:smelt_shared_test?mode=memory&cache=shared`) plus a leaked keep-alive connection, and runs all migrations — once per test binary. Shared-cache is what lets many `#[tokio::test]`s, each on its own runtime, see the same migrated schema; a plain `sqlite::memory:` is private per-connection.

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
