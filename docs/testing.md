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

## Sandbox tests

`src/sandbox.rs`'s tests hit a real Kubernetes API, the same "real
dependency, not a mock" posture as the database tests above — there's no
cheap way to fake the k8s API surface the way `anthropic::stream`'s mock
upstream fakes a single HTTP endpoint. Unlike the database tests, there's
no `#[sqlx::test]`-equivalent macro giving automatic per-test isolation, so
each test generates its own unique pod name (a timestamp-based suffix, not
a real UUID — see `uuid_like()` in `sandbox::tests`) to avoid colliding
with other tests or concurrent runs, and is responsible for its own
cleanup (explicit `manager.delete(sandbox)`, or in the one test that
covers the Drop path deliberately, a bounded `tokio::time::timeout` poll
waiting for the background drain task to do it instead).

Requires `KUBECONFIG` set and pointing at a reachable cluster with the
`smelt-park` namespace's RBAC applied (see
[docs/projects/plans/k8s-sandbox.md](projects/plans/k8s-sandbox.md)) —
`docker compose up -d k3s k3s-bootstrap` (or a full `docker compose up -d`)
sets this up automatically via `docker-compose.yml`'s `KUBECONFIG` env var
on the `smelt` service, pointing at the compose-provided `k3s` service.
Point it at `.kubeconfig.yaml` instead to run the same tests against the
real `homelab` cluster as a manual drift check — not something `cargo
test` does by default.

A real, non-obvious gotcha proven the hard way: deleting a sandbox pod
with Kubernetes' default `DeleteParams` leaves it `Terminating` for its
full grace period (commonly 30s) before it actually disappears, because a
plain `sleep infinity` container doesn't trap `SIGTERM`. Every delete in
`sandbox.rs` — including test cleanup — goes through
`immediate_delete_params()` (`grace_period_seconds: Some(0)`) specifically
to avoid tests timing out on this.

Two more, both proven the hard way on `sandbox-oom` (hit once during that
project's design spikes, then hit *again*, independently, while writing
its final integration test — worth internalizing rather than
rediscovering a third time):

- **An `AttachedProcess` (`pods.exec(...)`'s return value) whose
  stdout/stderr are never read can leave the remote command stalled
  rather than actually running**, not just buffered-and-ignored. If a
  test doesn't care about the output, it still needs to drain it (spawn a
  task that reads stdout/stderr to completion, or at minimum polls them)
  rather than dropping the handles unread.
- **Dropping the `AttachedProcess` itself — not just its split-off
  stdout/stderr handles — closes the underlying exec session**, and for a
  process that's directly attached (not `setsid`-detached the way
  `sandbox_agent`'s own injection launch is — see `inject_and_launch`),
  the container runtime kills it right along with the disconnect. A
  `{ let exec = pods.exec(...).await?; ...spawn readers off exec.stdout()/stderr()...}`
  block that lets `exec` fall out of scope at the end kills the remote
  process as soon as that block ends, often well before the command has
  actually done anything. Keep the whole `AttachedProcess` alive for as
  long as the remote command needs to run — e.g. move it (not just its
  stream handles) into the task that drains it, so the exec session stays
  open until that task itself finishes.

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

`ANTHROPIC_BASE_URL` is process-global, so **every** test across the whole binary that points it at a mock upstream must hold `anthropic::test_support::lock_anthropic_base_url()` (a `#[cfg(test)]`-only `std::sync::Mutex<()>` in `anthropic/mod.rs`) for the duration — `anthropic::stream`'s and `api::chat`'s mock-upstream tests both do. Without it, two such tests on different OS threads can each set the var to their own mock server's address and race, with one test's HTTP client ending up pointed at the other's server; when one file's tests need more than one mock-upstream scenario, prefer folding them into a single `#[tokio::test]` function (see `anthropic::stream`'s tests) over adding another test that also touches the lock, to keep contention low. The pure parsing logic (`interpret_stream_event`, deciding what a single decoded SSE payload means) is tested separately and synchronously, with no network or async runtime involved at all.

A mock upstream that needs to return a *different* response per call (e.g. a tool-use turn, then a follow-up turn once the tool result comes back) tracks a request count with a shared `AtomicUsize` in the route closure and indexes into a `Vec<String>` of bodies, clamped to the last one once exhausted — see `api::chat`'s `start_mock_upstream` test helper.

## Testing code that touches the process-global DB pool

Most server logic takes `pool: &PgPool` explicitly and uses `#[sqlx::test]`, per "Database tests" above. `api::chat::run_turn` is the one exception worth calling out: it was *changed* to take `pool: &PgPool` (rather than reaching for `db::get()` internally, which is what `send_message` itself still does) specifically so its own tests could use `#[sqlx::test]`. The first version reached for `db::get()` directly and initialized it once via a shared `tokio::sync::OnceCell` across tests — it worked in isolation but reliably deadlocked/timed out (`PoolTimedOut`) when multiple such tests ran concurrently, because each `#[tokio::test]` gets its *own* tokio runtime, and a `sqlx::PgPool`'s connections become unusable once the runtime that created them is torn down (which happens as soon as the test that happened to initialize the pool finishes) — a later test reusing the same process-global pool object from a *different* runtime hangs waiting for a connection that will never come back. Threading `pool: &PgPool` through instead sidesteps this: every test gets its own runtime-local, `#[sqlx::test]`-isolated pool, same as everywhere else. Any new server-side function that a background task might call (as `run_async`'s spawned task calls `run_turn`) should take its pool the same way, for the same reason.

## Testing async background-task behavior

`anthropic::tools`'s `run_async`/task-management-suite tests construct a `PgPool` via `PgPool::connect_lazy(...)` against a bogus URL rather than a real `#[sqlx::test]` pool — lazy construction never dials out until first use, and these tests only exercise registry logic (task creation, status transitions, cancellation), never the actual push-a-notification-through-`run_turn` path, so the pool is present (satisfying the type) but never really needs to connect. Tests that *do* need the push to actually land — proving a background task's notification round-trips through real persistence — live in `api::chat` instead, using a real `#[sqlx::test]` pool end-to-end (`anthropic::tools::execute`'s `pool: &PgPool` parameter carries the same real pool all the way into `run_async`'s spawned task).

When testing a code path that could plausibly deadlock (a lock re-acquired somewhere non-obvious, a channel nobody drains), wrap the call in `tokio::time::timeout(...)` and assert it doesn't elapse — a hung test otherwise just stalls the suite with no useful failure message. `api::chat::tests::test_run_turn_does_not_deadlock_when_model_calls_cancel_task` is a concrete example: it exists because `cancel_task` pushing its notification via an *awaited* `run_turn` call deadlocked against the per-conversation lock the *calling* `run_turn` was already holding — caught by exactly this pattern, fixed by detaching that one push with `tokio::spawn` instead.

**Task ids must be unique across the whole test binary, not just within one test.** `anthropic::tools`'s `TASKS` registry is a single `static` `HashMap<String, Task>` — shared by every test that runs in the same process, not scoped per test the way `#[sqlx::test]`'s pool is. Two different tests calling `run_async` with the same string task id (e.g. both using `"toolu_foo"`) race on the same `HashMap` key when run concurrently (the default), and whichever inserts last silently clobbers the other's registry entry — the loser's `wait_task`/`cancel_task`/etc. calls then observe a `Task` that isn't the one they started (wrong `AbortHandle`, wrong `Notify`, wrong everything), which reads as a mysterious hang or timeout with no useful error, not a clean failure. This bit two of `echo`/`write_task_stdin`'s own tests during development (`"toolu_echo"` and `"toolu_fast_add"` each reused across two different test functions) — the fix was giving every test's `run_async` calls their own distinct id. `tool_use_id`s passed to *other* tools (`cancel_task`, `wait_task`, `task_status`, ...) aren't at risk the same way — those tools only look at `input.task_id`, not the outer `tool_use_id` argument, so reusing a generic id like `"toolu_x"` across many tests for *those* calls is fine.

## Running tests

```bash
cargo test --features server                 # the real (server-gated) tests
cargo test --features server -- --nocapture   # show println! output
cargo test --features server test_name        # a single test by name
```

Most logic lives behind the `server` feature; plain `cargo test` compiles but skips it. See [Definition of done](development-process.md#definition-of-done) for the full two-target check.

## Browser verification

A small automated browser test tier exists (`src/browser_tests.rs`, see below) for behavior that genuinely needs a real DOM to verify — everything else is still a manual/scripted pass, driving a real headless Chrome instance against `dx serve --fullstack`.

### Playwright (preferred)

The dev container image bakes in a Python Playwright install specifically so this doesn't have to be rebuilt or asked for per session — see the `Dockerfile`'s `/opt/playwright-venv` stage:

```bash
dx serve --fullstack &                             # start the app (see setup.md)

/opt/playwright-venv/bin/playwright install chromium   # once per container instance —
                                                         # the venv exists in the image,
                                                         # but the browser binary itself
                                                         # downloads into ~/.cache/ms-playwright
                                                         # on first use

/opt/playwright-venv/bin/python your_script.py      # a short sync_playwright() script:
                                                     # launch chromium(args=["--no-sandbox"]),
                                                     # goto/click/fill, .screenshot(path=...)
```

Then view the screenshot (the `Read` tool renders images directly). This is a plain Python script per check, not a fixed CLI — see any recent UI-change conversation in this project for concrete examples (navigating to a conversation, clicking a sidebar entry, reading back `scrollTop`/`scrollHeight` via `page.eval_on_selector`, etc.).

### `scripts/browser-check/` (fallback)

Before Playwright was added to the image, UI verification in this sandbox had no browser, no Node, and no Python `pip` available at all (see the `delete-conversations` and `tool-use-round-trip` retrospectives) — `scripts/browser-check/` is a from-scratch, pure-stdlib driver built to cover that gap, and is kept as the fallback for an environment that still lacks Docker-rebuild/root access:

```bash
scripts/browser-check/setup.sh                     # once — downloads a headless
                                                     # Chrome-for-Testing binary and
                                                     # its shared libraries into
                                                     # .browser-check-cache/ (gitignored,
                                                     # never committed); idempotent,
                                                     # safe to re-run, no root needed

python3 scripts/browser-check/browser_check.py \
    http://127.0.0.1:8080/ \
    --screenshot /tmp/out.png \
    --action "click:.conversation-item" \
    --action "sleep:1000" \
    --action "scroll:.messages"
```

`scripts/browser-check/cdp.py` hand-rolls just enough raw WebSocket framing (RFC6455) to speak the Chrome DevTools Protocol directly, and `setup.sh` fetches Chrome for Testing plus its missing shared libraries (nss, atk, dbus, X11, mesa, ...) via non-root `apt-get --print-uris` + `dpkg-deb -x` into a local prefix — no root, no system package state touched. `--action` runs steps in order: `click:SELECTOR`, `type:SELECTOR=TEXT`, `wait:SELECTOR` (poll up to 10s), `scroll:SELECTOR` (scrolls to bottom), `sleep:MS`, `eval:JS` (escape hatch — also handy for injecting synthetic markup to preview CSS for a state you don't have live data for, e.g. an error variant when nothing's currently failing). Each run launches its own Chrome and kills it on exit unless `--keep-open` is passed, specifically so repeated runs don't leak orphaned processes the way plain `kill $pid` on `dx serve` itself can (`dx serve`'s actual Axum server runs as a *child* process under a different PID — killing only the `dx` wrapper leaves it running; `pkill -f 'target/dx/.*/server-'` or checking `ps aux` after is worth doing regardless of which tool started it).

### `src/browser_tests.rs` (automated)

A `#[cfg(test)]` module in the main binary crate (not a `tests/` integration test — this project has no `lib.rs`, so an external test binary couldn't reach `db`/`sandbox`/`anthropic::tools` at all), built and run under its own Cargo feature so it never slows down the default loop:

```bash
scripts/browser-check/setup.sh           # once — see above, this reuses the same
                                          # chrome-headless-shell download, not a
                                          # separate one
dx build --platform web                  # once per frontend change — dioxus-server's
                                          # serve_dioxus_application needs a pre-bundled
                                          # WASM/assets directory (target/dx/smelt/debug/
                                          # web/public) that only the dx CLI produces;
                                          # plain `cargo build`/`cargo test` never builds
                                          # it. The harness points DIOXUS_PUBLIC_PATH at
                                          # this directory (dioxus-server's own escape
                                          # hatch) rather than requiring `dx serve` to
                                          # already be running — discovered the first
                                          # time this test actually ran, not anticipated
                                          # up front.

cargo test --features "server browser-test" -- --ignored --test-threads=1
```

`#[ignore]`d by default (needs the two setup steps above, plus a real Postgres and k3s cluster reachable the same way every other real-cluster test already assumes) and deliberately just the one test — see `docs/projects/completed/20260815-sandbox-visibility.md` for the design and reasoning (in-process server via a factored-out `build_router()`, `chromiumoxide` talking directly to `chrome-headless-shell` over CDP rather than a `chromedriver`/WebDriver setup this environment doesn't have). Reaches into `db`/`sandbox`/`anthropic::tools` directly to set up scenarios (bypassing the model entirely — this tier verifies the browser/live-event pipeline, not tool-selection behavior) and asserts against the rendered DOM via `page.evaluate("document.body.innerText...")`, not screenshots.

## What's not covered yet

- **The automated browser tier is minimal, not comprehensive.** One test, covering the `sandbox-visibility` panel specifically — not a general framework other features are expected to plug into yet, and nothing runs it in CI (no CI exists in this repo at all). Worth extending once it's proven stable and another feature has a similar need for real-DOM verification.
- **No native SSR component-test harness.** Components aren't unit-tested by rendering them to a string outside a real page load. Worth adding if/when component logic grows complex enough that manual browser verification alone becomes slow to iterate on.
