# Testing

## Structure

Tests are `#[cfg(test)]` modules of the code they cover, not a separate `tests/*.rs` tree (smelt is a binary crate, so an integration test couldn't reach its `pub(crate)` items). A small module keeps them inline; once a file would cross the size ceiling (see [process/rules-commits.md](process/rules-commits.md#files-stay-under-the-ceiling)) they move to `foo/tests.rs`, declared `#[cfg(test)] mod tests;`, or to topic files `foo/tests/<topic>.rs` that start `use super::*;`, with the shared helpers in `foo/tests/mod.rs`. A test's helpers are visible to it either way, because a child module sees its parents' private items.

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

`#[sqlx::test]` connects to the Postgres server at `DATABASE_URL`, creates a new database per test, runs all migrations against it, and tears it down afterward — no shared fixture, no manual setup/teardown, and no cross-test interference since each test is fully isolated. This requires a reachable Postgres server while running tests (`docker compose up -d postgres`).

## Sandbox tests

In [testing/sandbox.md](testing/sandbox.md).

## Testing the Anthropic streaming client without the network

In [testing/techniques.md](testing/techniques.md).

## Testing spans (SME-137)

In [testing/techniques.md](testing/techniques.md).

## Testing code that touches a process-global resource across `#[tokio::test]` runtimes

In [testing/techniques.md](testing/techniques.md).

## Testing for deadlocks

In [testing/techniques.md](testing/techniques.md).

## Tests that touch per-conversation state

In [testing/techniques.md](testing/techniques.md).

## Running tests

```bash
scripts/check.sh                              # the per-commit gate (see below)
scripts/cluster-test --features server                 # the real (server-gated) tests
scripts/cluster-test --features server -- --nocapture   # show println! output
scripts/cluster-test --features server -- --exact sandbox::tests::manager::test_name   # exactly one test
```

**Any run that includes a `#[sqlx::test]` or a real-cluster test goes through `scripts/cluster-test`** (SME-100), which takes its arguments as `cargo test` does: it builds them with `--no-run` first, unlocked, then runs the same command under the cluster lock. A `cargo test … --no-run` needs no lock. See [personas/rules.md](personas/rules.md) ("One cluster user at a time").

**The cluster lock** is `flock` on `<git common dir>/smelt-cluster.lock`, taken by `scripts/with-cluster-lock COMMAND…`, which `check.sh`, `browser-tier`, `cluster-test` and `clean-test-namespace.sh` call around their test run only:
- while another process holds it, it prints `waiting for the cluster lock (held by pid N, <command>, in <directory>)` once, then waits;
- the command inherits the lock, as under the old outer `flock`, and runs in its caller's process group, so it can read a terminal and Ctrl-C ends it and its caller. The lock is held until the command and everything it started that still has the descriptor have ended: no kill of `with-cluster-lock` frees it while a test binary may still be running (SME-100 review 2). A server meant to outlive a run closes the descriptor itself, as `check-server` does for its `dx serve`;
- run inside a holder (a script that locks calling another, or an old outer `flock` in any form), it runs inside that lock rather than deadlocking, and for an outer `flock` says to drop it. It finds holders through `/proc/<pid>/fdinfo` and `/proc/locks`, Linux only, and the waiting line names a process holding the lock even after the one that took it has exited;
- `--output FILE` sends the command's output to FILE while the waiting line stays on the terminal (`check.sh` keeps `target/check/server-tests.log` this way);
- each run appends a line to `smelt-cluster-lock.log` next to the lock: start time, `waited=` and `held=` seconds, `exit=`, the worktree, the command;
- `SMELT_LOCK_DIR` moves the lock (and `check-server`'s port locks) elsewhere; the script tests that share `scripts/lib/test-check-scripts.sh` set it so their cases never touch the real one.

A name filter matches every test whose path contains it, and several filters are OR'd, so a filter meant for one unit test can also pick up real-cluster tests (on SME-85, `test_a_` matched a claims test that creates pods). To run one test, name it with `--exact` and its full path.

**A real-cluster test runs only this way, by exact name through `scripts/cluster-test`** (see [process/tdd.md](process/tdd.md#testing-rules)): on SME-115 a substring filter run outside the lock picked up a new test whose unfixed form was the bug being fixed, and it marked another run's claims in `smelt-park-test` for deletion.

**A module with both pure and real-cluster tests keeps the real-cluster ones in a `cluster` submodule** (as `sweep::tests::cluster::…` does), so a filter on the pure tests' path can't reach them. On SME-134 the filter `sweep::tests::test_the_sweep_`, meant for the pure tests, also ran four real-cluster sweep tests outside the lock.

**Gate from a detached gate worktree** when the lock queue is long, so writing the next commit goes on while a gate waits, and nothing edits the tree being gated (see [process/rules-commits.md](process/rules-commits.md)). A worktree has its own `target/`, so the gate's build never shares artefacts with the branch's:

```bash
git worktree add --detach ~/smelt-worktrees/gate-<ticket> <commit>
cd ~/smelt-worktrees/gate-<ticket>
scripts/check.sh > <scratchpad>/sme-N/gate-<commit>.log 2>&1; rc=$?   # act on rc, not on the log; it locks only its test run
# a commit without scripts/with-cluster-lock (before SME-100) takes no lock: build first, then
# flock "$(git rev-parse --git-common-dir)/smelt-cluster.lock" scripts/check.sh > … ; rc=$?
git checkout --detach <next commit>   # the next gate, once this one is back
```

Name the commit by hash (`git rev-parse --short <branch>`, run anywhere), never `HEAD`: in the gate worktree `HEAD` is its own last gated commit, not the branch's tip (SME-135).

Commit on top of a commit only once its gate passed. Run the browser tier from the gate worktree too. Remove the gate worktree (`git worktree remove`) when the branch merges, with its other worktrees.

Most logic lives behind the `server` feature; plain `cargo test` compiles but skips it.

`scripts/check.sh` is what every commit is gated on (`scripts/check.sh && git commit ...`, see [process/rules-commits.md](process/rules-commits.md)): the web build (`cargo check` for `wasm32-unknown-unknown`), the server binary build, the lint, the test build, and then, under the cluster lock, the server tests. It fails on any failure, including a warning in either build. It doesn't run the browser tier. The server tests' full log is kept at `target/check/server-tests.log` until the next run.

**The scripts a session runs for the gate and for hands-on checks delete no files** (`check.sh`, `browser-tier`, `check-server`, `build-sandbox-image.sh`, `with-cluster-lock`, `cluster-test`, `clean-test-namespace.sh`): Claude Code's safety check refuses a command whose scripts remove files at paths it can't resolve, and only a person can approve it (SME-107). Logs go to fixed paths under `target/`, overwritten by the next run (`browser-tier`'s `dx build` log is `target/browser-tier/web-bundle.log`), and `check-server` empties its state files (`.check-server.pid`, `.check-server.scratch-db` and, since SME-129, `.check-server.kubeconfig`, the cluster the server was started with, in the check worktree) rather than removing them: an empty or missing one means nothing is running, there's no scratch database to clean up or drop, or the cleanup tool should use the inherited `KUBECONFIG`. It also takes a lock on the scratch database's name (`$SMELT_LOCK_DIR/smelt-check-scratch-<name>.lock`, SME-129), held by a start until its pid file is written and by a stop through its cleanup, so the two can't act on the database at once. Dropping scratch databases and stopping processes stay. `scripts/test-gate-scripts`, `scripts/test-check-server` and `scripts/test-cluster-lock` check both, and the locks (what each gate script runs locked and unlocked, re-entry, `check-server`'s port lock and its scratch lock), with stubs for `cargo`, `dx`, `sqlx`, `curl`, `python3`, the image check and the cleanup tool, shared in `scripts/lib/test-check-scripts.sh`; `scripts/check.sh` runs all three, and you can run the one for the script you changed.

`mcp::tests::client::test_live_exa_search_through_smelt_mcp_client` checks the built-in Exa MCP server against the real service: smelt's own MCP client connects keylessly, sees only `web_search_exa`, and gets results back. It needs the internet and depends on Exa's unpublished free limits, so it's `#[ignore]`d **and** skips unless `SMELT_LIVE_EXA=1` is set. CI's browser job runs every ignored test, and this one shouldn't depend on Exa there. Run it with `SMELT_LIVE_EXA=1 cargo test --features server live_exa -- --ignored`. See [Definition of done](process/done.md) for the full two-target check.

## Browser verification

In [testing/browser.md](testing/browser.md).

## What's not covered yet

- **The automated browser tier covers what its scenarios list, not every page.** It's not a general framework other features are expected to plug into; add a scenario when a change needs real-DOM verification that should keep running. `webfetch.rs`'s separate test covers real navigation, the sandbox route, and the browsing-session tools (below).
- **The live browsing panel's input and frame wiring has no automated coverage** — only a manual pass with a real app and Playwright, driving a real model through a real conversation, confirmed it renders live frames and forwards input correctly. Browser scenario 15 checks the panel's layout, nothing more. The *tools* underneath it (`browsing.rs`'s own session/navigate/click/fill/screencast/input-dispatch logic) are automated-tested inside `webfetch.rs`'s real-browser test, and the panel's pure frontend logic (`chat/panels/browsing.rs`'s `browser_input_event_for_key`, `cdp_modifiers`, `frame_point`, `wheel_delta_pixels`, `coalesce_mouse_moves`, `address_bar_value`) has direct unit tests. The real-browser scenarios include regressions for what the branch's review found: a plain hover reaching the page with `buttons == 0`, Enter submitting a form, a viewer leaving and another arriving with no gap in frames, a subscription from a closed session not affecting the next one, the frame stream releasing its viewer when dropped, two racing `open_session` calls where exactly one wins, a `data:` URL being refused, a typed password never appearing in the element list, `fill` replacing (and clearing) a field instead of appending, Shift+Tab and Ctrl+Backspace keeping their modifiers, a second viewer getting a frame from a static page, and a close issued mid-open winning. Also: a click waiting for a delayed update and for a slow page it navigates to; `fill` with accented, CJK, emoji and multi-line text; and nothing a page does — a popup, a `target=_blank` link, a WebSocket, a service worker — reaching a refused address, in both `browsing` and `webfetch`. That last check needs an address the guard refuses but that is still reachable, and the tests' guard allows loopback. So it listens on this machine's own private address instead (found by opening a UDP socket toward a private range, which sends nothing) and fails loudly if the machine has none. The address bar's URL tracking is covered too: a URL update for the model navigating, a link click, a `pushState` change, and a refused load (which must report the address asked for, not `chrome-error://`). Browser-data isolation is covered from both sides: a cookie and a localStorage value set in one conversation's session must be visible to that session (so the check can't pass vacuously), invisible to another conversation's, and gone after a close and reopen. A second `webfetch` call must not see what a first one set. `webfetch::browser_tests::test_chrome_exits_with_its_owning_process` checks that Chrome can't be orphaned. It re-runs the test binary as a child process, which then owns a shared browser (via the `chrome_owner_helper` test, a no-op unless `SMELT_CHROME_OWNER_HELPER` is set). The test finds that child's Chrome through `/proc`, SIGKILLs the child and asserts Chrome exits too. The static-page scenario navigates somewhere with no focused input on purpose: a blinking caret keeps Chrome sending frames, which hid this bug from the first version of the test. What's still manual-only is the actual RSX event wiring (mouse/wheel handlers reading `element_coordinates()` and scaling through `frame_point`, the frame `<img>` reactively updating off the live stream), since automating "one headless browser watching another headless browser's live video feed and clicking on it" is a meaningfully bigger lift than `browser_tests.rs`'s existing scenarios. Worth a real automated scenario later, not assumed away.
- **No native SSR component-test harness.** Components aren't unit-tested by rendering them to a string outside a real page load. Worth adding if/when component logic grows complex enough that manual browser verification alone becomes slow to iterate on.
