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

## Testing a fetch's retries and bounds (SME-470)

The server's own fetches of a text file (mason's registry and Helix's `languages.toml` for the LSP catalog, models.dev's price catalog) go through `src/http_fetch.rs`'s `get_text`. Its tests run against local stand-ins with a hit counter, no network: a 503 or 429 then a 200 (2 hits), a 400 or 404 (1 hit), an always-500 (`attempts` hits), a stalled server that accepts and never answers (fails at the policy's deadline), and a 200 whose body is cut off partway (fails with 1 hit: no retry once a body began). Tests give a `Policy` of 10 ms pauses; the callers' own tests (`lsp::catalog::tests`, `pricing::tests`) ride out one 503 at the real policies' first pause (1 s and 2 s).

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
scripts/check.sh                              # the gate: per push point (see below)
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

**The script tests (`scripts/test-*`) run outside the cluster lock,** so another worktree's gate can run the same script at the same moment. Whatever a script test checks machine-wide must belong to its own run alone: files under `target/<script name>/` or from `mktemp`, ports from `free_port`, and process names that carry the run's pid and are matched up to the space after it. Never a fixed name that `pgrep -f` would also find in the other run (SME-203: `test-ci-web-bundle`'s fixed `sme67-fake-dx` failed gates whenever two runs overlapped; it is now `sme203-fake-dx-<pid>`).

A name filter matches every test whose path contains it, and several filters are OR'd, so a filter meant for one unit test can also pick up real-cluster tests (on SME-85, `test_a_` matched a claims test that creates pods). To run one test, name it with `--exact` and its full path.

**Looking past the `park` account:** the tests' kubeconfig is the `park` service account's, which can't read `kube-system` (the `local-path` provisioner and its helper pods) or storage classes. On the host, `scripts/k3s-admin-kubeconfig` writes this stack's k3s admin kubeconfig to `target/k3s-admin.yaml`, gitignored, with its server set to `https://k3s:6443`, for a session in the `smelt` service to read. It's cluster-admin on the local test cluster only.

**A real-cluster test runs only this way, by exact name through `scripts/cluster-test`** (see [process/tdd.md](process/tdd.md#testing-rules)): on SME-115 a substring filter run outside the lock picked up a new test whose unfixed form was the bug being fixed, and it marked another run's claims in `smelt-park-test` for deletion.

**A module with both pure and real-cluster tests keeps the real-cluster ones in a `cluster` submodule** (as `sweep::tests::cluster::…` does), so a filter on the pure tests' path can't reach them. On SME-134 the filter `sweep::tests::test_the_sweep_`, meant for the pure tests, also ran four real-cluster sweep tests outside the lock.

**Ids that name cluster objects come from `src/id_alloc.rs` bands (SME-255), never from the clock.** `GLOBAL_IDS` (`[2e9, 3e9)`) backs `db::clear_ids_base()` — every harness move and the production fresh-database start; `FIXTURE_IDS` (`[3e9, 4e9)`, tests only) backs every hand-picked fixture id, session-name suffix and test offset. Each band is one cursor per process, seeded once from nanos mixed with the pid and advanced by 100,000 a call, so no two callers in a process share a value and consecutive ones sit far apart; a test that wants two disjoint ids calls it twice. The test path's move (`test_support::start_ids_clear_of_other_runs`) moves *every* sequence in the database, not a list, so a table whose ids name cluster objects needs no registration. The harness sweep (SME-134) additionally takes, best-effort, objects that predate the current process by 2 s, not just ones older than an hour, so a killed run's leftovers go at the next run's start whatever their names — the arm is off for a run whose start `/proc` can't place, or places ahead of the clock, and the sweep's log line names both bounds whether that arm ran or not. `src/repo_lints/id_alloc.rs` fails the gate on a new clock-fold id site or `uuid_like`-style helper.

### Known infrastructure failures

A gate red **only** on one of these is not the branch's: re-run it before investigating it as code (each one costs a full gate and an investigation to rediscover). File what a re-run confirms per the rules — a known failure is still filed the first time — and when a red matches an entry but reads differently, add the new symptom to the entry rather than opening a fresh one-off (SME-254, finding 3; `docs/personas/developer.md` step 4 names the five acts).

- **The shared-cluster start/stop race family — [SME-255](https://linear.app/smelt-agent/issue/SME-255) + [SME-256](https://linear.app/smelt-agent/issue/SME-256), one family, kept as one entry.** The id half is closed: `clear_ids_base()` folded nanos through a modulo and could hand two parallel workers the same conversation id, pod names and all (SME-255 — replaced by the `src/id_alloc.rs` bands above, the move-every-sequence harness move, and the sweep's predates-the-process arm). What a red gate can still meet is the 256 half: a Stop that deletes the cluster object before it closes the record lets a start in that window report Kubernetes' 404 instead of "the sandbox was stopped while it was starting". If a red reads like a pod-name collision anyway — a `NotOurs { …, Foreign }` or a 409 on `sandbox-<id>` — treat it as the family's third symptom and file it; the allocation fix should have made it impossible.
- **The k3s DNS failure in the cluster LSP tests** (met in [SME-185](https://linear.app/smelt-agent/issue/SME-185)'s gating): name resolution inside the test cluster fails mid-run, or the language-server install itself dies on a network error (met again in SME-255's gating: `curl: (56) … bad record MAC` and go's proxy `tls: bad record MAC` mid-download, green when each test re-ran alone); re-run before reading it as the change. The install downloads from GitHub releases read as `curl: (22)` naming a `504` too — met three times on 2026-11-05: PR #268's CI attempt 1 (run 42045040104) and the local gates of SME-395's fix round and SME-396's. The same 504 met a second site on 2026-11-08: CI's start-stack compose-plugin download, on fresh ARC runner pods that had lost the plugin, failing both test jobs in 10 s before any test ran (run 42470920569; [SME-458](https://linear.app/smelt-agent/issue/SME-458)). That site no longer needs a re-run: `scripts/ci-install-compose` takes a sha256-checked copy from the runner's cache volume first, then 3 GitHub attempts, then `docker/compose-bin`, and one `install-compose:` line in its step names the path taken (`setup.md`'s CI section). A `github releases attempt k of 3 failed: curl: (22) … 504` line followed by `fetched` is this flake survived; only `::error::install-compose: every source failed (…)` is a red, an outage of GitHub releases and Docker Hub at once. The Docker Hub pulls of the catalog images (the `tls: bad record MAC` rust:1 pull of [SME-391](https://linear.app/smelt-agent/issue/SME-391)) stopped being a mid-suite dependency when `scripts/k3s-bootstrap.sh` began pre-pulling them at cluster bootstrap (SME-391): if that pre-pull is what fails, the bootstrap says so loudly before the suite starts — a stack-side re-run (`docker compose up --exit-code-from k3s-bootstrap k3s-bootstrap`), not a test investigation.
- **`scripts/test-cluster-lock`'s signal cases** (met in [SME-186](https://linear.app/smelt-agent/issue/SME-186)'s gating): `after TERM to its holder, the lock came free while the command still ran` — a race in `with-cluster-lock`'s TERM path, unrelated to any script a branch changes ([SME-260](https://linear.app/smelt-agent/issue/SME-260)); the same race reads on `KILL` too (met in SME-255's gating: `after KILL to its holder, the lock came free while the command still ran`); run the script alone before reading it as one's own.
- **`#[sqlx::test]` setups failing with `failed to connect to setup test database: PoolTimedOut`** (sqlx-core 0.8.6 `testing/mod.rs:226`; [SME-376](https://linear.app/smelt-agent/issue/SME-376), six at once in CI run 41783718251). This failure is fixed, not one to re-run: a new sighting is a ticket. sqlx makes every test database through one master pool. That pool has 20 connections, and a setup waits at most 30 s for one; sqlx hard-codes both numbers, with no setting for either. Every passing test ends in sqlx's `DROP DATABASE`, which in Postgres forces an immediate checkpoint and waits for it while it holds a master connection. On CI's runner, where the cluster tests share the disk, those checkpoints' fsyncs took up to 10 s, with a dozen drops waiting on one together. In the failing run nothing finished for 24 s, then three tests finished in the same 50 ms, and the setups queued behind the drops passed 30 s. The failure reproduces every time against a scratch Postgres whose checkpointer is stopped (`kill -STOP`) for 75 s while 24 test threads run. CI's Postgres now runs with `fsync=off` (`.github/ci-compose.yml`): it lives for one job, and without fsync a busy disk can't stretch its checkpoints. The server job's "Postgres checkpoints and slow statements" step prints each run's longest checkpoints and the statements over 5 s, and warns when any statement took over 5 s (a timed checkpoint paces itself on purpose, so the warning watches the statements it would keep waiting). The first run with the fix (42162549334) spent 1 s in fsync over the suite, against 177 s before, and logged no statement over 5 s. Locally the tests share the dev database's Postgres, which keeps fsync on; its checkpoints average about 0.2 s.
- **`check-server` stop failing transiently**: retry it until it succeeds, with a pause and a time limit, exactly as [personas/rules.md](personas/rules.md)'s hands-on block shows; one still failing after the limit goes in the report, never left unsaid, and the scratch database is kept.

**Gate from a detached gate worktree** when the lock queue is long, so writing the next change goes on while a push point's gate or a bisect waits, and nothing edits the tree being gated (see [process/rules-commits.md](process/rules-commits.md)). A worktree has its own `target/`, so the gate's build never shares artefacts with the branch's:

```bash
git worktree add --detach ~/smelt-worktrees/gate-<ticket> <commit>
cd ~/smelt-worktrees/gate-<ticket>
scripts/check.sh > <scratchpad>/sme-N/gate-<commit>.log 2>&1; rc=$?   # act on rc, not on the log; it locks only its test run
# a commit without scripts/with-cluster-lock (before SME-100) takes no lock: build first, then
# flock "$(git rev-parse --git-common-dir)/smelt-cluster.lock" scripts/check.sh > … ; rc=$?
git checkout --detach <next commit>   # a bisect's next gate, once this one is back
```

Name the commit by hash (`git rev-parse --short <branch>`, run anywhere), never `HEAD`: in the gate worktree `HEAD` is its own last gated commit, not the branch's tip (SME-135).

Push only once the push point's gate passed. Run the browser tier from the gate worktree too. Remove the gate worktree (`git worktree remove`) when the branch merges, with its other worktrees.

Most logic lives behind the `server` feature; plain `cargo test` compiles but skips it.

`scripts/check.sh` is what every push point is gated on (`scripts/check.sh && git push ...`), before the PR opens and at each review round's tip, while each commit gets its module's tests and both builds (see [process/rules-commits.md](process/rules-commits.md)): the web build (`cargo check` for `wasm32-unknown-unknown`), the server binary build, the lint, the test build, and then, under the cluster lock, the server tests. It fails on any failure, including a warning in either build or in the test build, which alone compiles `#[cfg(test)]` code (SME-361). CI's test steps check their logs' compile sections the same way (`scripts/ci-no-warnings`), since their `cargo test` builds with no `-D warnings`, and `browser-tier` checks its own test build, the only local build that compiles the `browser-test` feature's code. It doesn't run the browser tier. The server tests' full log is kept at `target/check/server-tests.log` until the next run.

**The scripts a session runs for the gate and for hands-on checks delete no files** (`check.sh`, `browser-tier`, `check-server`, `build-sandbox-image.sh`, `with-cluster-lock`, `cluster-test`, `clean-test-namespace.sh`): Claude Code's safety check refuses a command whose scripts remove files at paths it can't resolve, and only a person can approve it (SME-107). Logs go to fixed paths under `target/`, overwritten by the next run (`browser-tier`'s `dx build` log is `target/browser-tier/web-bundle.log`), and `check-server` empties its state files (`.check-server.pid`, `.check-server.scratch-db` and, since SME-129, `.check-server.kubeconfig`, the cluster the server was started with, in the check worktree) rather than removing them: an empty or missing one means nothing is running, there's no scratch database to clean up or drop, or the cleanup tool should use the inherited `KUBECONFIG`. It also takes a lock on the scratch database's name (`$SMELT_LOCK_DIR/smelt-check-scratch-<name>.lock`, SME-129), held by a start until its pid file is written and by a stop through its cleanup, so the two can't act on the database at once. Dropping scratch databases and stopping processes stay. `scripts/test-gate-scripts`, `scripts/test-check-server` and its sibling suites `scripts/test-check-server-scratch`, `test-check-server-guards`, `test-check-server-db-inputs`, `test-check-server-ports` and `test-check-server-previews`, `scripts/test-cluster-lock` and `scripts/test-dotenv-value` check both, and the locks (what each gate script runs locked and unlocked, re-entry, `check-server`'s port lock, its bounded wait for a port another check server holds and its scratch lock), with stubs for `cargo`, `dx`, `sqlx`, `curl`, `python3`, the image check and the cleanup tool, shared in `scripts/lib/test-check-scripts.sh` — `test-dotenv-value` needs none of them, being the `.env` reader's own table of fixtures (SME-186); `scripts/check.sh` runs all of them, and you can run the one for the script you changed. Their scratch clones under `target/` (the check worktrees, `repo-env`) are made once and reused through `shared_clone`, which points a reused clone's alternate at this checkout's object store before taking it: a `--shared` clone records that store as an absolute path, so one made on the host at `/workspace/smelt` failed at once under the compose service's `/app`, the same `target/` through the bind mount (SME-251). One that still can't read its HEAD is named for a person to remove. Every suite that sources `test-check-scripts.sh` records the HEAD and branch of the checkout it runs in and fails at `finish` if the run moved either: `check-server` checks the served ref out in its check worktree, and a `CHECK_WORKTREE` that isn't a working tree of its own moved the repository around it until SME-265 refused that shape (its `start` names the directory and exits 1).

**The browser tier bounds its `== web bundle` step** (SME-457). It runs
`scripts/ci-web-bundle` — not a bare `dx build` — under
`timeout -k 30 ${SMELT_BROWSER_TIER_BUNDLE_TIMEOUT:-1800}`: the watcher
handles SME-67's post-client silence with a warning and exports
`RUSTUP_AUTO_INSTALL=0`; the cap catches what the watcher deliberately leaves
open — silence *before* the client bundle — and fails naming the step, the
cap and where to look. A red there is a hung build, never a slow one: a warm
bundle is ~15 s, a cold in-stack one minutes.

The hang this fixed at its mechanism, and what to look for when a `dx build`
goes silent: dioxus-cli's build estimates its progress display with
`cargo +nightly rustc --unit-graph -Z unstable-options …`, and the stack image
has no nightly; unless `RUSTUP_AUTO_INSTALL=0` is set (the gate scripts and
the stack env set it — `setup.md`'s toolchain-posture note), rustup's `cargo`
shim answers a missing toolchain by downloading one from `RUSTUP_DIST_SERVER`
with **no timeout**. On a path that accepts but never answers, the estimate
child hangs, `dx build` waits on it forever, and no line is ever logged —
while `check.sh` stays green, because it never runs `cargo +nightly`. On a
hung dx the tell is the child `cargo +nightly rustc --unit-graph …` in state
`S` (`wchan=futex_wait`) at ~0 CPU, with an fd at
`…/usr/local/rustup/tmp/<rand>_file` and a socket. To reproduce on demand,
point `RUSTUP_DIST_SERVER` at a listener that accepts and sleeps: a bundle
through `scripts/ci-web-bundle` completes against it (the estimate fails in
ms and dx ignores it), a bare `dx build --platform web` — guard cleared, as
below — does not:

```bash
docker compose run -d --name sme457-hole --no-deps --entrypoint python3 smelt -c \
  'import socket,time
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(("0.0.0.0",8123)); s.listen(128)
while True: time.sleep(60)'
hole=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' sme457-hole)
docker compose run --rm --no-deps -T smelt bash -c \
  "timeout 120 env -u RUSTUP_AUTO_INSTALL RUSTUP_DIST_SERVER=http://$hole:8123 dx build --platform web; echo rc=\$?"  # hangs: rc=124, silent (-u: the stack env sets the guard itself now)
docker compose run --rm --no-deps -T smelt bash -c \
  "RUSTUP_DIST_SERVER=http://$hole:8123 scripts/ci-web-bundle"  # green; warm ~15 s
docker rm -f sme457-hole
```

**dx's tools come from the image, and dx downloads none** (SME-413). The
Dockerfile's `dx-tools` stage installs wasm-bindgen (crates.io, at
Cargo.lock's version) and esbuild (npm, sha256-pinned); wasm-opt is Debian's
binaryen; `base` (the dev image) and `bundle-env` (the release build's) set
`NO_DOWNLOADS=1`, so dx takes them from PATH or fails with `Missing …`. A
`dx build` that logs `Installing wasm-bindgen-cli`, `esbuild` or `wasm-opt`
is running in a stale image: rebuild it (`docker compose build smelt`). A
hand `dx bundle --release` in the dev container needs `--debug-symbols false`
(dx defaults it to true, and Debian's wasm-opt 120 aborts on the DWARF it
keeps, so dx ships the wasm unoptimised with only an ERROR line), or use
`docker/server-bundle.sh`, which passes it and fails on a failed wasm-opt.
`scripts/test-dx-offline [server|dev|guard]`, run by hand on the seat's
host, bundles with GitHub, npm and static.rust-lang.org blocked and rustup's
dist server stood in for — the release bundle, CI's `scripts/ci-web-bundle`,
and the guard that fails a release build whose wasm-opt fails — and keeps
each leg's log under `target/test-dx-offline/`; `scripts/test-dx-offline
clean` removes its images and volumes (a release `target/` of several GB).

**A check server's previews are its own** (SME-96). `start` gives the server `SMELT_PREVIEW_ADDR=0.0.0.0:<CHECK_PORT+200>` (8281 for the default 8081), preview links on that port, and `SMELT_BASE_URL=http://localhost:<CHECK_PORT>`. So preview links and the pages that link back to a conversation work out of the box, and never reach the dev server's 8181 or Grafana's 8182. Only a `-- NAME=VALUE` override changes them. The environment and `.env` hold the dev server's values here (compose sets `SMELT_BASE_URL` for every shell), so `start` ignores them and names each one it ignored. Given only an address, the links follow its port; given a link template on another port, `start` warns. An empty `SMELT_PREVIEW_ADDR` or `SMELT_PREVIEW_URL` is refused, while an empty `SMELT_BASE_URL` is passed on. A preview port something already listens on is refused before anything is checked out, created or started, as the app's port is (SME-175). `start` prints what it chose, with whose each value is. `scripts/test-check-server-previews` covers it.

`mcp::tests::client::test_live_exa_search_through_smelt_mcp_client` checks the built-in Exa MCP server against the real service: smelt's own MCP client connects keylessly, sees only `web_search_exa`, and gets results back. It needs the internet and depends on Exa's unpublished free limits, so it's `#[ignore]`d **and** skips unless `SMELT_LIVE_EXA=1` is set. CI's browser job runs every ignored test, and this one shouldn't depend on Exa there. It's a `#[sqlx::test]`, since a connect reads its server's row (SME-167), so run it under the cluster lock: `SMELT_LIVE_EXA=1 scripts/cluster-test --features server -- --ignored --exact mcp::tests::client::test_live_exa_search_through_smelt_mcp_client`. See [Definition of done](process/done.md) for the full two-target check.

## Keeping the suite fast and green

SME-341 spent two days getting a branch that changes nothing back to a first-attempt green, and halving CI's time. These keep it there.

- **A red that the change can't explain is a ticket, never a re-run.** A red on `main`, or on a PR whose diff can't reach the failing test gets a Test reliability ticket with its signature: the test, the assert, the message, and the run. A flaky-test ticket is **Urgent** (the user, 2026-11-05): flaky tests don't accumulate. A flake is fixed as soon as it's spotted: in the ticket that met it when that fits, otherwise as an Urgent ticket that's taken before other work. It's fixed at the mechanism, never by `#[ignore]`, a deletion, or a timeout that happens to survive (SME-329's green-only rule). Find the mechanism before the fix: a diagnostic in a scratch copy, the failing state recorded, and, where the bug is a missing wait or guard, a mutant that removes it and turns the test red. **This holds for a local gate as much as for CI** (the user, 2026-11-08): a test red in `scripts/check.sh` that the change can't explain is filed Urgent, or its ticket linked, and fixed first, as SME-440 was; the push waits and is gated again once that fix is in, and a second run that comes back green is never the way past it.
- **Look for the ticket before the cause.** Before debugging a red, search the Test reliability project, Backlog included, for the test's name and its message, and start from the mechanism written there. When a filed flake bites a gate, fix it then if it's small. On SME-341 and the tickets after it, a red already filed with its mechanism was diagnosed again from scratch six times (SME-314, SME-250, SME-193, SME-303 and SME-260, SME-278: the 2026-11-02 retrospective).
- **A flake fixed at its mechanism is fixed everywhere that mechanism is, in the same PR.** Search for the helper, budget or pattern the fix changes, not only the failing test's file, and fix or rule out each other test that shares it; link or close any Backlog ticket that's the same mechanism. The test build's 1 s MCP connect budget made SME-193, then SME-356, then SME-363 red one test at a time, each fix covering only the tests in its own signature, while SME-352 sat in Backlog with the same race (SME-353, proposal 4). It's the flake fix's version of close-out's search for every other place that makes the same decision.
- **A flake ticket closes only when its mechanism is gone, and a recurrence reopens it** (the SME-385 retrospective). A ticket that has gone quiet isn't fixed. A sweep of stale flaky-test tickets cancels one only when the race or budget it traced is no longer in the code, and says which commit or ticket removed it; otherwise the ticket stays open at Urgent. A red whose signature matches a closed or cancelled flake ticket reopens that ticket, with the new run added, rather than starting a new one. The 2026-11-06 sweep cancelled SME-293, red again within 12 hours, and SME-281, whose race was still in the test and turned SME-450's gate red.
- **The suite has a time budget.** Each CI job that runs tests (`server`, `browser`) reports its test run's own time against a budget in its summary: server 600 s, browser 480 s (`scripts/ci-test-report`; about 270–350 s and 220 s at SME-341's close). Over budget is a warning, not a red, since a red for a slow pass would be a flake of its own. The same summary lists the server tests libtest saw run over 60 s and the browser tier's ten slowest scenarios. A new entry there, or a warning, is a ticket too.
- **A test waits on the state it needs, never a fixed timer or an earlier step's side effect.** The flakes SME-341 fixed were each a wait on the wrong thing: a reply's chunks counted before it opened, a message typed into a disabled box, a claim "held" by a pod that could never be scheduled (before SME-335, a container with no resources got the namespace LimitRange's 64Gi max as its request), a lock shared by two tests' conversation 1. A test that builds its own pod spec gives it a small request; a test that needs ids no other test uses takes them from `id_alloc::FIXTURE_IDS`.
- **Slow is usually waiting, not work.** Before parallelising, find what's being waited on: SME-341's pod tests were throttled by the test cluster's own volume provisioner (`scripts/k3s-bootstrap.sh`), not by CPU. `scripts/k3s-admin-kubeconfig` shows what the `park` account can't.

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
