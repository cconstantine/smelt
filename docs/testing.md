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

## Cost, size and budget tests

**A test that measures what a page, a read or a result costs needs a fixture where the cost exists** (SME-189). Its `read_file` tests each used a one-line or three-line file, so nothing ever asked what reading on within a long line costs when the file goes on behind it — and the fix shipped re-sending every line after the cut one, about twelve times what the walk wins. Only a code review saw it, four fix commits and two review rounds late. So when the thing under test is a budget, a cap or a truncation, put the interesting line in the middle of a full page and assert what the response *holds*, not only what it says: a minimal fixture can't see a per-page cost.

## Database tests

`db.rs`'s CRUD functions take `pool: &PgPool` as an explicit parameter (see [database.md](database.md)), so tests use `#[sqlx::test]` instead of `db::get()`'s process-wide pool — each test function gets its own freshly created, migrated Postgres database, handed in as an argument:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_thing_round_trip(pool: PgPool) {
        let c = create_conversation(&pool).await.expect("create");
        // ... exercise db functions against `c.id`, passing `&pool` explicitly
    }
}
```

`#[sqlx::test]` connects to the Postgres server at `DATABASE_URL`, creates a new database per test, runs all migrations against it, and tears it down afterward — no shared fixture, no manual setup/teardown, and no cross-test interference since each test is fully isolated. This requires a reachable Postgres server while running tests (`docker compose up -d postgres`).

**Every `#[sqlx::test]` names a guarded migrator** (SME-212): `migrator = "crate::test_lock::MIGRATOR"`, or `"crate::test_lock::NO_MIGRATIONS"` for a test that migrates by hand (the old `migrations = false`). sqlx hands the migrator over before it creates the test's database, and `src/test_lock.rs` refuses the test there unless `scripts/with-cluster-lock` runs it (see "The cluster lock" below), so a bare `cargo test` fails it by name, panicking at `src/test_lock.rs`, instead of dropping another run's databases. A bare attribute fails the gate: `repo_lints::tests::test_every_sqlx_test_takes_the_cluster_lock` tokenizes every tracked `.rs` file (`src/repo_lints/cluster_lock.rs`), leaving out comments and reading strings, raw strings and char literals whole, and names each `#[…]` that names the path `sqlx::test` wherever it sits (`#[::sqlx::test]`, inside a `cfg_attr`, after other code on its line, inside a macro call), and each `use` that imports `sqlx::test`, read to its `;` through any groups. The guard counts only as `migrator = "…"` among the attribute's own tokens, not in a comment. It reads source, not macro expansions: an attribute a macro assembles from fragments, or `sqlx` imported under another crate name, isn't seen.

**A test that holds several connections at once shares a pool with every other test.** Each `#[sqlx::test]` database is created through sqlx's one parent pool for the process, 20 connections, and a test's own pool draws on it too. A held-row race test (see [tdd.md](process/tdd.md)) holds about four at once: the held transaction, each caller under test, and the `pg_stat_activity` watcher. On SME-181 eight of them running together ran out and failed with `PoolTimedOut`. Such tests take turns through a process-wide `tokio::sync::Mutex` (a `static` the test locks first, as `git::server::trust_race_tests` does).

## Sandbox tests

In [testing/sandbox.md](testing/sandbox.md).

### The pod lifecycle tests (SME-94)

In [testing/sandbox.md](testing/sandbox.md#the-pod-lifecycle-tests-sme-94).

### Docker in the sandbox (SME-33)

In [testing/sandbox.md](testing/sandbox.md#docker-in-the-sandbox-sme-33).

### The sandbox agent's protocol (SME-53)

In [testing/sandbox.md](testing/sandbox.md#the-sandbox-agents-protocol-sme-53).

### Language servers (SME-35)

In [testing/sandbox.md](testing/sandbox.md#language-servers-sme-35).

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

**Any run that includes a `#[sqlx::test]` or a real-cluster test goes through `scripts/cluster-test`** (SME-100), which takes its arguments as `cargo test` does: it builds them with `--no-run` first, unlocked, then runs the same command under the cluster lock. A `cargo test … --no-run` needs no lock. See [personas/rules.md](personas/rules.md) ("One cluster user at a time"). The tests enforce it (SME-212): run any other way, every `#[sqlx::test]` and real-cluster test fails at once with "run it with scripts/cluster-test …" before touching Postgres or the cluster, while pure tests in the same run pass.

**The cluster lock** is `flock` on `<git common dir>/smelt-cluster.lock`, taken by `scripts/with-cluster-lock COMMAND…`, which `check.sh`, `browser-tier`, `cluster-test` and `clean-test-namespace.sh` call around their test run only:
- while another process holds it, it prints `waiting for the cluster lock (held by pid N, <command>, in <directory>)` once, then waits;
- the command inherits the lock, as under the old outer `flock`, and runs in its caller's process group, so it can read a terminal and Ctrl-C ends it and its caller. The lock is held until the command and everything it started that still has the descriptor have ended: no kill of `with-cluster-lock` frees it while a test binary may still be running (SME-100 review 2). A server meant to outlive a run closes the descriptor itself, as `check-server` does for its `dx serve`;
- run inside a holder (a script that locks calling another, or an old outer `flock` in any form), it runs inside that lock rather than deadlocking, and for an outer `flock` says to drop it. It finds holders through `/proc/<pid>/fdinfo` and `/proc/locks`, Linux only, and the waiting line names a process holding the lock even after the one that took it has exited;
- `--output FILE` sends the command's output to FILE while the waiting line stays on the terminal (`check.sh` keeps `target/check/server-tests.log` this way);
- each run appends a line to `smelt-cluster-lock.log` next to the lock: start time, `waited=` and `held=` seconds, `exit=`, the worktree, the command;
- the command runs with `SMELT_CLUSTER_LOCK_HELD` set to the lock file's path, which the tests' guard (`src/test_lock.rs`) checks (SME-212). The script never reads it, so a value in the caller's environment doesn't count as the lock. Never set it by hand or in `.env`: that turns the guard off;
- `SMELT_LOCK_DIR` moves the lock (and `check-server`'s port locks) elsewhere; the script tests that share `scripts/lib/test-check-scripts.sh` set it so their cases never touch the real one.

A name filter matches every test whose path contains it, and several filters are OR'd, so a filter meant for one unit test can also pick up real-cluster tests (on SME-85, `test_a_` matched a claims test that creates pods). To run one test, name it with `--exact` and its full path.

**A real-cluster test runs only this way, by exact name through `scripts/cluster-test`** (see [process/tdd.md](process/tdd.md#testing-rules)): on SME-115 a substring filter run outside the lock picked up a new test whose unfixed form was the bug being fixed, and it marked another run's claims in `smelt-park-test` for deletion.

**A module with both pure and real-cluster tests keeps the real-cluster ones in a `cluster` submodule** (as `sweep::tests::cluster::…` does), so a filter on the pure tests' path can't reach them. On SME-134 the filter `sweep::tests::test_the_sweep_`, meant for the pure tests, also ran four real-cluster sweep tests outside the lock.

### Known infrastructure failures

A gate red **only** on one of these is not the branch's: re-run it before investigating it as code (each one costs a full gate and an investigation to rediscover). File what a re-run confirms per the rules — a known failure is still filed the first time — and when a red matches an entry but reads differently, add the new symptom to the entry rather than opening a fresh one-off (SME-254, finding 3; `docs/personas/developer.md` step 4 names the five acts).

- **The shared-cluster start/stop race family — [SME-255](https://linear.app/smelt-agent/issue/SME-255) + [SME-256](https://linear.app/smelt-agent/issue/SME-256), one family, kept as one entry.** Two symptoms so far: `clear_ids_base()` (`2e9 + nanos % 1e9` — the modulo drops the second) can hand two parallel `#[sqlx::test]` workers the same conversation id, and since pod names derive from it, their pods collide by name in the shared cluster; and a Stop that deletes the cluster object before it closes the record lets a start in that window report Kubernetes' 404 instead of "the sandbox was stopped while it was starting". The gate log names the pod once, so the two cannot be told apart after the fact — check a red gate against the whole family, and look for the third symptom before filing a fourth ticket.
- **The k3s DNS failure in the cluster LSP tests** (met in [SME-185](https://linear.app/smelt-agent/issue/SME-185)'s gating): name resolution inside the test cluster fails mid-run; re-run before reading it as the change.
- **`scripts/test-cluster-lock`'s TERM case** (met in [SME-186](https://linear.app/smelt-agent/issue/SME-186)'s gating): `after TERM to its holder, the lock came free while the command still ran` — a race in `with-cluster-lock`'s TERM path, unrelated to any script a branch changes ([SME-260](https://linear.app/smelt-agent/issue/SME-260)); run the script alone before reading it as one's own.
- **`check-server` stop failing transiently**: retry it until it succeeds, with a pause and a time limit, exactly as [personas/rules.md](personas/rules.md)'s hands-on block shows; one still failing after the limit goes in the report, never left unsaid, and the scratch database is kept.

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

**The scripts a session runs for the gate and for hands-on checks delete no files** (`check.sh`, `browser-tier`, `check-server`, `build-sandbox-image.sh`, `with-cluster-lock`, `cluster-test`, `clean-test-namespace.sh`): Claude Code's safety check refuses a command whose scripts remove files at paths it can't resolve, and only a person can approve it (SME-107). Logs go to fixed paths under `target/`, overwritten by the next run (`browser-tier`'s `dx build` log is `target/browser-tier/web-bundle.log`), and `check-server` empties its state files (`.check-server.pid`, `.check-server.scratch-db` and, since SME-129, `.check-server.kubeconfig`, the cluster the server was started with, in the check worktree) rather than removing them: an empty or missing one means nothing is running, there's no scratch database to clean up or drop, or the cleanup tool should use the inherited `KUBECONFIG`. It also takes a lock on the scratch database's name (`$SMELT_LOCK_DIR/smelt-check-scratch-<name>.lock`, SME-129), held by a start until its pid file is written and by a stop through its cleanup, so the two can't act on the database at once. Dropping scratch databases and stopping processes stay. `scripts/test-gate-scripts`, `scripts/test-check-server`, `scripts/test-check-server-guards`, `scripts/test-cluster-lock` and `scripts/test-dotenv-value` check both, and the locks (what each gate script runs locked and unlocked, re-entry, `check-server`'s port lock, its bounded wait for a port another check server holds and its scratch lock), with stubs for `cargo`, `dx`, `sqlx`, `curl`, `python3`, the image check and the cleanup tool, shared in `scripts/lib/test-check-scripts.sh` — `test-dotenv-value` needs none of them, being the `.env` reader's own table of fixtures (SME-186); `scripts/check.sh` runs all of them, and you can run the one for the script you changed.

`mcp::tests::client::test_live_exa_search_through_smelt_mcp_client` checks the built-in Exa MCP server against the real service: smelt's own MCP client connects keylessly, sees only `web_search_exa`, and gets results back. It needs the internet and depends on Exa's unpublished free limits, so it's `#[ignore]`d **and** skips unless `SMELT_LIVE_EXA=1` is set. CI's browser job runs every ignored test, and this one shouldn't depend on Exa there. It's a `#[sqlx::test]`, since a connect reads its server's row (SME-167), so run it under the cluster lock: `SMELT_LIVE_EXA=1 scripts/cluster-test --features server -- --ignored --exact mcp::tests::client::test_live_exa_search_through_smelt_mcp_client`. See [Definition of done](process/done.md) for the full two-target check.

## Browser verification

In [testing/browser.md](testing/browser.md).

### Playwright (preferred)

In [testing/browser.md](testing/browser.md#playwright-preferred).

### `scripts/browser-check/` (fallback)

In [testing/browser.md](testing/browser.md#scriptsbrowser-check-fallback).

### `src/browser_tests.rs` (automated)

In [testing/browser-tier.md](testing/browser-tier.md).

## What's not covered yet

- **The automated browser tier covers what its scenarios list, not every page.** It's not a general framework other features are expected to plug into; add a scenario when a change needs real-DOM verification that should keep running. `webfetch.rs`'s separate test covers real navigation, the sandbox route, and the browsing-session tools (below).
- **The live browsing panel's input and frame wiring has no automated coverage** — only a manual pass with a real app and Playwright, driving a real model through a real conversation, confirmed it renders live frames and forwards input correctly. Browser scenario 15 checks the panel's layout, nothing more. The *tools* underneath it (`browsing.rs`'s own session/navigate/click/fill/screencast/input-dispatch logic) are automated-tested inside `webfetch.rs`'s real-browser test, and the panel's pure frontend logic (`chat/panels/browsing.rs`'s `browser_input_event_for_key`, `cdp_modifiers`, `frame_point`, `wheel_delta_pixels`, `coalesce_mouse_moves`, `address_bar_value`) has direct unit tests. The real-browser scenarios include regressions for what the branch's review found: a plain hover reaching the page with `buttons == 0`, Enter submitting a form, a viewer leaving and another arriving with no gap in frames, a subscription from a closed session not affecting the next one, the frame stream releasing its viewer when dropped, two racing `open_session` calls where exactly one wins, a `data:` URL being refused, a typed password never appearing in the element list, `fill` replacing (and clearing) a field instead of appending, Shift+Tab and Ctrl+Backspace keeping their modifiers, a second viewer getting a frame from a static page, and a close issued mid-open winning. Also: a click waiting for a delayed update and for a slow page it navigates to; `fill` with accented, CJK, emoji and multi-line text; and nothing a page does — a popup, a `target=_blank` link, a WebSocket, a service worker — reaching a refused address, in both `browsing` and `webfetch`. That last check needs an address the guard refuses but that is still reachable, and the tests' guard allows loopback. So it listens on this machine's own private address instead (found by opening a UDP socket toward a private range, which sends nothing) and fails loudly if the machine has none. The address bar's URL tracking is covered too: a URL update for the model navigating, a link click, a `pushState` change, and a refused load (which must report the address asked for, not `chrome-error://`). Browser-data isolation is covered from both sides: a cookie and a localStorage value set in one conversation's session must be visible to that session (so the check can't pass vacuously), invisible to another conversation's, and gone after a close and reopen. A second `webfetch` call must not see what a first one set. `webfetch::browser_tests::test_chrome_exits_with_its_owning_process` checks that Chrome can't be orphaned. It re-runs the test binary as a child process, which then owns a shared browser (via the `chrome_owner_helper` test, a no-op unless `SMELT_CHROME_OWNER_HELPER` is set). The test finds that child's Chrome through `/proc`, SIGKILLs the child and asserts Chrome exits too. The static-page scenario navigates somewhere with no focused input on purpose: a blinking caret keeps Chrome sending frames, which hid this bug from the first version of the test. What's still manual-only is the actual RSX event wiring (mouse/wheel handlers reading `element_coordinates()` and scaling through `frame_point`, the frame `<img>` reactively updating off the live stream), since automating "one headless browser watching another headless browser's live video feed and clicking on it" is a meaningfully bigger lift than `browser_tests`' existing scenarios. Worth a real automated scenario later, not assumed away.
- **No native SSR component-test harness.** Components aren't unit-tested by rendering them to a string outside a real page load. Worth adding if/when component logic grows complex enough that manual browser verification alone becomes slow to iterate on.
