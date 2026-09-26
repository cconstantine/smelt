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
`smelt-park`/`smelt-park-test` namespaces' RBAC applied (see
[SME-7](https://linear.app/smelt-agent/issue/SME-7)) —
`docker compose up -d k3s k3s-bootstrap` (or a full `docker compose up -d`)
sets this up automatically via `docker-compose.yml`'s `KUBECONFIG` env var
on the `smelt` service, pointing at the compose-provided `k3s` service.
Point it at `.kubeconfig.yaml` instead to run the same tests against the
real `homelab` cluster as a manual drift check — not something `cargo
test` does by default. These tests always run against `smelt-park-test`,
never the `smelt-park` namespace a real running `dx serve` dev instance
uses — `src/sandbox.rs`'s `NAMESPACE` constant resolves per `#[cfg(test)]`,
not an env var, specifically so a test run can't accidentally collide with
(or leave litter for) a real dev instance, or vice versa.

A real, non-obvious gotcha proven the hard way: deleting a sandbox pod
with Kubernetes' default `DeleteParams` leaves it `Terminating` for its
full grace period (commonly 30s) before it actually disappears, because
the sandbox container doesn't trap `SIGTERM`. Every delete in
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
  process that's directly attached (not `setsid`-detached), the container
  runtime kills it right along with the disconnect. A
  `{ let exec = pods.exec(...).await?; ...spawn readers off exec.stdout()/stderr()...}`
  block that lets `exec` fall out of scope at the end kills the remote
  process as soon as that block ends, often well before the command has
  actually done anything. Keep the whole `AttachedProcess` alive for as
  long as the remote command needs to run — e.g. move it (not just its
  stream handles) into the task that drains it, so the exec session stays
  open until that task itself finishes.

A related one from `sandbox-native-environment`'s generic-volumes work:
**a PVC still mounted by a pod carries Kubernetes' own
`kubernetes.io/pvc-protection` finalizer**, so deleting the PVC right after
deleting the pod that mounts it can leave `get_opt` still returning
`Some` (a `Terminating` object, not gone) — the finalizer only releases
once the pod is genuinely gone, not just marked for deletion. A test (or
any caller) that deletes both needs to poll for the *pod* to actually
disappear before deleting the PVC, and/or poll for the PVC itself to
disappear rather than checking once immediately after the delete call
returns — the delete API call succeeding doesn't mean the object is
already gone.
- **Writing a large payload to an `AttachedProcess`'s stdin while the
  executed command produces *zero* stdout output reliably breaks the
  connection (`BrokenPipe`) partway through the write.** Found on
  `sandbox-native-environment`'s registry-delivery spike: streaming a
  ~2.2MB tarball via `sh -c "cat > /tmp/image.tar"` (no stdout at all)
  failed every time; the identical payload against `sh -c "cat >
  /tmp/image.tar" && echo done` (one trailing line of stdout) succeeded
  reliably — bisected with a payload-size sweep and a real-vs-synthetic-
  bytes comparison before finding the actual variable was the command, not
  the data. Two things to carry forward: drain stdout/stderr *concurrently*
  with the stdin write (`tokio::join!`, not read-after-write), and make
  sure the executed command produces at least one byte of stdout if the
  payload is more than a few hundred KB — `src/bin/sandbox_image_import.rs`
  does both.

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

## Testing code that touches a process-global resource across `#[tokio::test]` runtimes

**General hazard, not just `PgPool`:** each `#[tokio::test]` fn gets its own independent tokio runtime. Any process-global resource (a `OnceLock`/`OnceCell`-held value) whose correctness depends on a background task that outlives a single call — a connection driven by a spawned task, a handler loop, anything with a "keep this running or the resource stops working" shape — breaks the same way once more than one `#[tokio::test]` fn touches it: whichever test's runtime first initialized it also owns that background task, and once *that* runtime tears down (at the end of *that* test fn), the resource silently stops working for every other test still trying to reuse it, from a different runtime. Seen twice now — `PgPool` below, and `chromiumoxide::Browser` (`src/webfetch.rs`'s own real-browser test, which hit "send failed because receiver is gone" the first time it split its scenarios into three separate `#[tokio::test]` fns instead of one) — check for this before adding a third. The fix is the same shape both times: either thread the resource through explicitly so each test gets its own runtime-local instance (`PgPool`'s fix), or consolidate every scenario that needs to share one instance into a single `#[tokio::test]` fn (`chromiumoxide::Browser`'s fix, matching `src/browser_tests.rs`'s own already-established "deliberately one test, not several" pattern).

Most server logic takes `pool: &PgPool` explicitly and uses `#[sqlx::test]`, per "Database tests" above. `api::chat::run_turn` is the one exception worth calling out: it was *changed* to take `pool: &PgPool` (rather than reaching for `db::get()` internally, which is what `send_message` itself still does) specifically so its own tests could use `#[sqlx::test]`. The first version reached for `db::get()` directly and initialized it once via a shared `tokio::sync::OnceCell` across tests — it worked in isolation but reliably deadlocked/timed out (`PoolTimedOut`) when multiple such tests ran concurrently, because each `#[tokio::test]` gets its *own* tokio runtime, and a `sqlx::PgPool`'s connections become unusable once the runtime that created them is torn down (which happens as soon as the test that happened to initialize the pool finishes) — a later test reusing the same process-global pool object from a *different* runtime hangs waiting for a connection that will never come back. Threading `pool: &PgPool` through instead sidesteps this: every test gets its own runtime-local, `#[sqlx::test]`-isolated pool, same as everywhere else. Any new server-side function that a background task might call (as `run_async`'s spawned task calls `run_turn`) should take its pool the same way, for the same reason.

**The same hazard inside one test: dioxus's worker runtimes.** `dioxus-server` runs server functions and SSR on a `LocalPoolHandle`, worker threads with a Tokio runtime each. A pooled database connection, or a cluster client connection, first opened while handling a request is tied to that worker's runtime. In a test that runs the app in-process (`browser_tests.rs`), once the server shuts down those runtimes go with it, and a later query on the test's own runtime can pick up such a connection: it either fails ("A Tokio 1.x context was found, but it is being shutdown") or hangs forever. Whether it does depends on which connection the pool hands out, so it shows up as a flaky failure or a CI job that never finishes. Do all database and cluster checks before shutting the in-process server down (SME-39).

## Testing async background-task behavior

`anthropic::tools`'s `run_async`/task-management-suite tests construct a `PgPool` via `PgPool::connect_lazy(...)` against a bogus URL rather than a real `#[sqlx::test]` pool — lazy construction never dials out until first use, and these tests only exercise registry logic (task creation, status transitions, cancellation), never the actual push-a-notification-through-`run_turn` path, so the pool is present (satisfying the type) but never really needs to connect. Tests that *do* need the push to actually land — proving a background task's notification round-trips through real persistence — live in `api::chat` instead, using a real `#[sqlx::test]` pool end-to-end (`anthropic::tools::execute`'s `pool: &PgPool` parameter carries the same real pool all the way into `run_async`'s spawned task).

When testing a code path that could plausibly deadlock (a lock re-acquired somewhere non-obvious, a channel nobody drains), wrap the call in `tokio::time::timeout(...)` and assert it doesn't elapse — a hung test otherwise just stalls the suite with no useful failure message. `api::chat::tests::test_run_turn_does_not_deadlock_when_model_calls_cancel_task` is a concrete example: it exists because `cancel_task` pushing its notification via an *awaited* `run_turn` call deadlocked against the per-conversation lock the *calling* `run_turn` was already holding — caught by exactly this pattern, fixed by detaching that one push with `tokio::spawn` instead.

**Task ids must be unique across the whole test binary, not just within one test.** `anthropic::tools`'s `TASKS` registry is a single `static` `HashMap<String, Task>` — shared by every test that runs in the same process, not scoped per test the way `#[sqlx::test]`'s pool is. Two different tests calling `run_async` with the same string task id (e.g. both using `"toolu_foo"`) race on the same `HashMap` key when run concurrently (the default), and whichever inserts last silently clobbers the other's registry entry — the loser's `wait_task`/`cancel_task`/etc. calls then observe a `Task` that isn't the one they started (wrong `AbortHandle`, wrong `Notify`, wrong everything), which reads as a mysterious hang or timeout with no useful error, not a clean failure. This bit two of `echo`/`write_task_stdin`'s own tests during development (`"toolu_echo"` and `"toolu_fast_add"` each reused across two different test functions) — the fix was giving every test's `run_async` calls their own distinct id. `tool_use_id`s passed to *other* tools (`cancel_task`, `wait_task`, `task_status`, ...) aren't at risk the same way — those tools only look at `input.task_id`, not the outer `tool_use_id` argument, so reusing a generic id like `"toolu_x"` across many tests for *those* calls is fine.

## Tests that touch per-conversation state

Some state is process-wide and keyed by conversation id: the turn lock, a stop, the pause after a stop, and whether a turn is running (`api::chat`'s `CONVERSATION_LOCKS`, `TURN_STOPS`, `PAUSED`, `TURNS_IN_FLIGHT`), and the event channels. But every `#[sqlx::test]` database numbers conversations from 1, and tests run in parallel, so two tests' "conversation 1" are the same key.
- **A test that stops or pauses a conversation, or asserts on its events,** creates it with `db::create_conversation_with_id(pool, <a unique high id>)`. On `pod-management`, before this, one test's stop paused every other test's conversation 1, and five unrelated wake and notice tests failed.
- **Tests that wait for one kind of event** skip the others: turns now publish `TurnState` too.

## Running tests

```bash
cargo test --features server                 # the real (server-gated) tests
cargo test --features server -- --nocapture   # show println! output
cargo test --features server test_name        # a single test by name
```

Most logic lives behind the `server` feature; plain `cargo test` compiles but skips it.

`mcp::tests::test_live_exa_search_through_smelt_mcp_client` checks the built-in Exa MCP server against the real service: smelt's own MCP client connects keylessly, sees only `web_search_exa`, and gets results back. It needs the internet and depends on Exa's unpublished free limits, so it's `#[ignore]`d **and** skips unless `SMELT_LIVE_EXA=1` is set. CI's browser job runs every ignored test, and this one shouldn't depend on Exa there. Run it with `SMELT_LIVE_EXA=1 cargo test --features server live_exa -- --ignored`. See [Definition of done](development-process.md#definition-of-done) for the full two-target check.

## Browser verification

A small automated browser test tier exists (`src/browser_tests.rs`, see below) for behavior that genuinely needs a real DOM to verify — everything else is still a manual/scripted pass, driving a real headless Chrome instance against `dx serve --fullstack`.

**A CSS/asset edit made while `dx serve` is already running doesn't reliably reach a *fresh* page load.** `App`'s `asset!("/assets/chat.css")` resolves to a content-hashed bundle path (`/assets/chat-<hash>.css`) baked into the served HTML at build time; `dx`'s hot-reload pushes a live patch over its dev websocket to tabs that were already open when the edit happened, but a brand-new browser instance (exactly what a screenshot script launches each run) requests the hashed URL fresh and can get a stale pre-edit bundle if the server hasn't actually rebuilt yet. Found on `sandbox-native-environment`: a CSS addition looked hot-reloaded (the log even said so) but a fresh `browser_check.py` run kept rendering unstyled markup until `dx serve` was killed (both the wrapper *and* its child `target/dx/.../web/server-*` process — killing just the wrapper leaves the child running, same gotcha the `scripts/browser-check/` section below already documents for a different reason) and restarted for a real full rebuild. If a browser-verification screenshot doesn't reflect a CSS change that should be there, restart `dx serve` before assuming the change itself is wrong.

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

`#[ignore]`d by default (needs the two setup steps above, plus a real Postgres and k3s cluster reachable the same way every other real-cluster test already assumes) and deliberately just the one test for this file's own scope — see SME-10 for the design and reasoning (in-process server via a factored-out `build_router()`, `chromiumoxide` talking directly to `chrome-headless-shell` over CDP rather than a `chromedriver`/WebDriver setup this environment doesn't have). Reaches into `db`/`sandbox`/`anthropic::tools` directly to set up scenarios (bypassing the model entirely — this tier verifies the browser/live-event pipeline, not tool-selection behavior) and asserts against the rendered DOM via `page.evaluate("document.body.innerText...")`, not screenshots.

**The page is styled only because the harness serves the stylesheet itself.** A plain `cargo test` build doesn't bundle assets: `asset!("/assets/chat.css")` resolves to the source file's own path (`/app/assets/chat.css`), which neither the bundle nor dioxus-server serves, so until SME-40 every page in this tier ran unstyled and any layout measurement there was meaningless. The harness now routes that exact URL to `assets/chat.css`, and scenario 1 asserts the stylesheet loads. A new asset referenced with `asset!()` needs the same treatment before a scenario can rely on it.

`src/webfetch.rs` has its own separate `#[ignore]`d, `browser-test`-gated real-browser test (`webfetch::browser_tests::test_fetch_scenarios`) — same `chrome-headless-shell` binary/setup, same `cargo test --features "server browser-test" -- --ignored --test-threads=1` invocation runs both, but a different module/concern (a feature's own browser-driving + SSRF-guard logic, not DOM/panel rendering), so it isn't a scenario folded into `browser_tests.rs`'s one test. Also different in one real way: it launches its *own* browser instance via `webfetch`'s real (production) `shared_browser`, not `browser_tests.rs`'s own harness — and, having hit the cross-runtime hazard above first-hand, is deliberately still just the one `#[tokio::test]` function covering all its scenarios sequentially, not several.

**A piped `cargo test` invocation can look hung when it's actually finished.** If a test spawns a long-lived child process (a shared browser, kept running by design rather than torn down per-test), that child inherits and can hold open any stdio the parent process didn't explicitly close or redirect — `chromiumoxide` only pipes `chrome-headless-shell`'s stderr, not its stdout, so the browser keeps the test binary's own stdout fd alive for as long as it runs. Piping `cargo test`'s output through another command that waits for real EOF (`| tail`, `| grep`, ...) then blocks forever, even after `cargo test` itself (and the Rust test process) have already cleanly exited — not a code bug, a shell-pipeline artifact. Redirect to a real file (`> output.log 2>&1`) instead when a test launches anything long-lived; a file read doesn't block waiting for every writer to close.

## What's not covered yet

- **The automated browser tier is minimal, not comprehensive.** `browser_tests.rs`'s one test covers the `sandbox-visibility` panel, (since `auto-compaction`) the context-usage indicator/detail view/compaction divider, (since `todo-list-tool`) the todo panel, a reply streaming into one conversation staying there when the viewer switches mid-stream (the model is a slow mock upstream on `ANTHROPIC_BASE_URL`, the one scenario that does call `send_message`), including the sidebar picking up the new title; and (since the bug bash) a reply the tab didn't send reaching it live, a missing conversation saying so, and a background-notification error clearing on a switch; `webfetch.rs`'s own separate test covers real navigation, SSRF-guard behavior, and (since `web-browsing`) the underlying browsing-session tools (navigate/click/fill/go_back/screencast frames/input dispatch) — not a general framework other features are expected to plug into yet. It does run in CI now (`.github/workflows/ci.yml`, see [development-process.md](development-process.md#definition-of-done)), with every `browser_tests.rs` scenario bypassing the model (seeding state directly, never a real `send_message`) since neither this environment nor CI has real Anthropic credentials (the cross-conversation scenario points the model at a local mock instead). Since `pod-management`, it also covers the sidebar's live pod dot and stopping a pod from `/pods` (scenario 12), and stopping a turn, including Stop appearing in a tab that didn't send (scenario 13). **Keep the number of open smelt tabs low: close tabs a scenario is done with.** Over HTTP/1.1 the browser allows 6 connections per host across all tabs. Each chat tab holds one always-open stream, and a tab's own loading needs a free connection besides its stream (for its snapshot requests). So about five smelt tabs is the most the harness can have open at once: past that, a new tab never finishes loading. Both this and a Stop click queuing behind a reply's own stream (fixed on `connection-limits`, when replies moved onto the conversation stream) were hit while writing scenarios 12–14. Scenario 14 covers replies streaming to every tab, a reload mid-reply, the sender seeing its message once, and five tabs leaving room for Stop. **Before typing into or clicking the page, wait for the WASM client to be live** (`wait_for_live_client`, or `wait_for_resource` on a page without a conversation). The server-rendered page accepts typing before hydration with no handlers attached, so input is silently lost; the harness page also permanently shows `dx`'s "Your app is being rebuilt" overlay, which is a red herring. The signal is the client's last post-subscription snapshot request completing. The event stream itself never completes, so it never shows up in resource timings. **The harness cleans up after itself, even when a scenario fails.** It runs against the real dev database and cluster, so every conversation a scenario creates is recorded (`new_conversation`) and removed at the end, pods first, the same way deleting a conversation in the app does. The test then checks nothing was left. That cleanup has to run *before* `harness.shutdown()`: once the harness has shut down, the cluster client's connection is gone ("runtime dropped the dispatch task") and pod deletes silently fail. Leftover pods matter: enough of them and new pods stop starting (`create_pod: Timeout`, then `ProtocolSwitch(500)` on terminals). Other tests can still leave some behind (a crashed run, `sandbox.rs`'s OOM tests); `scripts/clean-test-namespace.sh` clears the namespace. Worth extending further once another feature has a similar need for real-DOM verification.
- **The live browsing panel's DOM/RSX wiring itself has no automated coverage** — only a manual `dx serve` + Playwright pass, driving a real model through a real conversation, confirmed it renders live frames and forwards input correctly. The *tools* underneath it (`browsing.rs`'s own session/navigate/click/fill/screencast/input-dispatch logic) are automated-tested via `webfetch.rs`'s real-browser test, and the panel's pure frontend logic (`chat.rs`'s `browser_input_event_for_key` keydown mapping and `coalesce_mouse_moves`) has direct unit tests. The real-browser scenarios include regressions for what the branch's review found: a plain hover reaching the page with `buttons == 0`, Enter submitting a form, a viewer leaving and another arriving with no gap in frames, a subscription from a closed session not affecting the next one, the frame stream releasing its viewer when dropped, two racing `open_session` calls where exactly one wins, a `data:` URL being refused, a typed password never appearing in the element list, `fill` replacing (and clearing) a field instead of appending, Shift+Tab and Ctrl+Backspace keeping their modifiers, a second viewer getting a frame from a static page, and a close issued mid-open winning. Also: a click waiting for a delayed update and for a slow page it navigates to; `fill` with accented, CJK, emoji and multi-line text; and nothing a page does — a popup, a `target=_blank` link, a WebSocket, a service worker — reaching a refused address, in both `browsing` and `webfetch`. That last check needs an address the guard refuses but that is still reachable, and the tests' guard allows loopback. So it listens on this machine's own private address instead (found by opening a UDP socket toward a private range, which sends nothing) and fails loudly if the machine has none. The address bar's URL tracking is covered too: a URL update for the model navigating, a link click, a `pushState` change, and a refused load (which must report the address asked for, not `chrome-error://`). Browser-data isolation is covered from both sides: a cookie and a localStorage value set in one conversation's session must be visible to that session (so the check can't pass vacuously), invisible to another conversation's, and gone after a close and reopen. A second `webfetch` call must not see what a first one set. `webfetch::browser_tests::test_chrome_exits_with_its_owning_process` checks that Chrome can't be orphaned. It re-runs the test binary as a child process, which then owns a shared browser (via the `chrome_owner_helper` test, a no-op unless `SMELT_CHROME_OWNER_HELPER` is set). The test finds that child's Chrome through `/proc`, SIGKILLs the child and asserts Chrome exits too. The static-page scenario navigates somewhere with no focused input on purpose: a blinking caret keeps Chrome sending frames, which hid this bug from the first version of the test. What's still manual-only is the actual RSX event wiring (mouse/wheel handlers reading `element_coordinates()`, the frame `<img>` reactively updating off the live stream), since automating "one headless browser watching another headless browser's live video feed and clicking on it" is a meaningfully bigger lift than `browser_tests.rs`'s existing scenarios. Worth a real automated scenario later, not assumed away.
- **No native SSR component-test harness.** Components aren't unit-tested by rendering them to a string outside a real page load. Worth adding if/when component logic grows complex enough that manual browser verification alone becomes slow to iterate on.
