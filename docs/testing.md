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

`#[sqlx::test]` connects to the Postgres server at `DATABASE_URL`, creates a new database per test, runs all migrations against it, and tears it down afterward — no shared fixture, no manual setup/teardown, and no cross-test interference since each test is fully isolated. This requires a reachable Postgres server while running tests (`docker compose up -d postgres`).

## Sandbox tests

`src/sandbox/tests.rs`'s tests hit a real Kubernetes API, the same "real
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
uses — `src/sandbox/mod.rs`'s `NAMESPACE` constant resolves per `#[cfg(test)]`,
not an env var, specifically so a test run can't accidentally collide with
(or leave litter for) a real dev instance, or vice versa.

A real, non-obvious gotcha proven the hard way: a deleted pod stays
`Terminating` for its full grace period before it actually disappears.
smelt's own deletes use `pod_delete_params()` (20s, so the pod's dockerd
can stop its containers; SME-51 B6), and `create_pod` waits for a
previous pod to go. Test cleanup that needs nothing to stop cleanly uses
`immediate_delete_params()` (`grace_period_seconds: Some(0)`) instead, so
tests don't time out waiting.

Three about `pods.exec`, the first two proven the hard way on `sandbox-oom` (hit once during that
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

### The pod lifecycle tests (SME-94)

`src/sandbox/pod_lifecycle_tests.rs` runs the sandbox's own operations against real pods, one feature per test: pod creation and its guards, terminals, a finished command waking the model, the file tools, glob and grep, repos and project instructions, crash detection, the user's stop, the pods view, `watch_pods`, claims and volumes. A failure names the feature, and the rest still run. Each test:

- starts with `own_sandbox(&pool)`: `db::test_support::start_ids_clear_of_other_runs` moves its conversation, pod and volume ids to a base no other test or run shares (pods, claims and labels are named after them, in one shared namespace), and `use_test_manager` gives `sandbox::get()` on the test's thread a manager of its own (see the cross-runtime hazard below);
- runs its scenario through `run_then_tear_down(&pool, &client, limit, async { ... })`, which bounds it in time, then deletes every pod and claim of its database's conversations, every volume's claim and anything else labelled with its database's instance, pass, fail or timeout, before re-raising. First it takes each conversation's pod-start lock and keeps it: `create_pod` starts a pod in a task of its own, which a cut-off scenario leaves running, and it would otherwise make its pod after the deletes. That waits out every start already running or queued (for at most two running waits plus a minute, then it gives up and says so) and blocks any asked for afterwards;
- sets up the state it needs itself, never what another test left;
- waits its turn: at most four run their scenarios at once (`SCENARIOS_AT_ONCE`, outside the time limit). A dozen pods with Docker sidecars starting together on one node outran a pod start's 90 s wait, failing these and the other real-cluster tests.

They don't take the turn-test lock: their conversations' ids are clear of every other test's. The volume test's pod, made outside a conversation, is deleted by the test itself before it asserts, and by the instance sweep if it doesn't get that far. A killed run leaves its objects in its own id range, which no later run reuses; the sweep below deletes them once they're an hour old.

**The harness sweeps old leftovers (SME-134).** The first `own_sandbox` in a test process sweeps `smelt-park-test` (`src/sandbox/pod_lifecycle_tests/sweep.rs`), and later ones wait for it. It deletes every pod and claim there at least an hour old, whatever its name, id or instance label: no test keeps an object anywhere near that long, and every object in the namespace is a test's. It leaves anything already being deleted, and any old claim that a pod it keeps still mounts, or a pod it chose but couldn't delete (made again under its name, or a failed delete). It lists claims before pods, so a pod made between the two listings is seen with the claim it mounts. Each delete is held to the uid the object was listed with, so an object already gone (404) or deleted and made again under its name since the listing (409) is skipped, and two processes can sweep at once. It is bounded at 60 s, never fails a test, and prints one summary line (plus one for each object it was refused or failed to delete), written past the test harness's output capture so a passing test shows it too (`scripts/check.sh` keeps it in `target/check/server-tests.log`, not on its own output): `swept N pods, M claims older than 1h from smelt-park-test (K kept: mounted by a newer pod; …)`. Every `scripts/check.sh` and CI run sweeps; the browser tier runs only `--ignored` tests and doesn't, so what a tier run leaves goes at a later gate. Three guards keep it out of `smelt-park`, the live namespace:

1. the module is `#[cfg(test)]`, so no binary contains it, and no script calls it;
2. its `Api`s are built only from the literal `SWEEP_NAMESPACE`, after `check_namespace` refuses `smelt-park` or any mismatch with the harness's `NAMESPACE`;
3. `choose` drops any object whose own `metadata.namespace` isn't `smelt-park-test`.

`test_the_sweep_names_only_the_test_namespace` and `test_the_sweep_never_chooses_an_object_outside_the_test_namespace` pin guards 2 and 3. The sweep's own real-cluster tests label what they make with a value of their own and sweep only that label, so even an age of zero reaches nothing of another run's.

**A test-side wait for something smelt bounds by an env-configurable timeout is computed from that timeout,** never a literal sized from the local default: CI raises some of them (`SANDBOX_RUNNING_WAIT_TIMEOUT_SECS=120` in `ci.yml`). Teardown's wait for a pod start is `2 * running_wait_timeout()` plus a minute for this reason; on SME-94 a literal 150 s, sized from the local default, was shorter than a start can take in CI.

### Docker in the sandbox (SME-33)

- `test_docker_in_a_sandbox_pod_works_and_stays_inside_the_pod` and `test_an_oom_in_a_nested_container_restarts_only_the_docker_sidecar` run real containers. Their base image is the sandbox's own files (`sudo tar -C / -c bin sbin lib lib64 usr etc | docker import - local/base`), so no test pulls from Docker Hub. Leave out `usr/lib64` or `etc` and a container fails with `exec /usr/bin/sh: no such file or directory`.
- An OOM test needs memory that's actually written: `head -c 900M /dev/zero | tail` holds it, Python's `bytearray(n)` doesn't (its zero pages are never allocated, so nothing is killed). After the sidecar's OOM kill, a restarted dockerd reports the old containers as `Exited (255)`, often before the pod's status shows the restart.
- Each test's checks run inside `catch_unwind`, so the pods and claim it made are deleted even when an assertion fails; a panic otherwise unwinds past the cleanup.
- Run `scripts/build-sandbox-image.sh` after changing the agent (`src/bin/sandbox_agent.rs`, `src/agent_protocol.rs`, `src/docker_net.rs`), its dependencies (`Cargo.toml`, `Cargo.lock`) or `docker/sandbox/`: the pod runs the imported image, not the tree you just built. The image is named after the agent sources it was built from, `smelt-sandbox:src-<first 16 hex of scripts/agent-sources-hash>` (`scripts/sandbox-image-ref`), and `scripts/check.sh`, `scripts/browser-tier`, `scripts/check-server` and CI run their pods from it through `SANDBOX_IMAGE` (SME-102). So a branch that changes the agent builds and tests its own image while the image the dev server runs, named after the main checkout's sources, stays as it is; a fresh worktree with unchanged agent sources needs no build, because the image named after those sources is already in the cluster, whoever built it (SME-84). A server with no `SANDBOX_IMAGE` runs the image named after the sources it was built from (`build.rs` computes the same name, and `sandbox::tests::test_with_no_setting_the_image_is_the_one_named_after_this_trees_agent_sources` keeps it equal to the script's; SME-121), so the gate that merges an agent change has usually already built the image the dev server will want. `--latest` also moves `smelt-sandbox:latest`, which nothing uses by default; a persona never passes it. `scripts/cluster-doctor` (run by `check.sh` and `browser-tier`) says when the image is missing: it checks, through `sandbox_image_import --check <references>` (the read-only `ctr images ls -q`), that the node has the image the tests will use and the Docker sidecar's. The kubelet deletes images no pod is using once the node's disk passes 85%, and every pod then fails with `ErrImageNeverPull`; old `src-<hash>` images go the same way.

### The sandbox agent's protocol (SME-53)

- Neither end needs a cluster to be tested. `sandbox_agent`'s `socket_tests` run its real router on a loopback port with real `bash` terminals and a tungstenite client. `sandbox::agent_connection_tests` put a fake agent on a loopback port behind `dialer_for` (keyed by pod id, so parallel tests don't meet) and script its hello and replies, for smelt's side: timeouts, a dropped connection, outdated agents, two callers connecting at once, a teardown during a connect. They use `#[sqlx::test]` for the pod's row; `pod_row` moves the pod id sequence so the registry, which is keyed by pod id, never sees two tests' pods as one.
- Change `src/agent_protocol.rs` and the fixture test (`src/fixtures/agent_protocol_v1.jsonl`) fails until the change is either additive (bump the minor, add an example) or a new major with a new fixture file. `PROTOCOL_VERSION`'s comment says which is which.
- `pod_lifecycle_tests` are where the real agent runs in a real pod: rebuild the image after changing either end of the protocol, or their pods come up with an agent smelt reports as outdated.
- The agent's `test_an_unparseable_message_is_not_logged_verbatim` captures logs through one process-wide subscriber limited to the agent's own target: a per-thread subscriber misses events whose callsite another test's thread registered first, and tungstenite's trace logs dump every frame the test client sends.

### Language servers (SME-35)

- `src/lsp/pods.rs`'s cluster tests use a stand-in sandbox (the sandbox image, idle, with the conversation's workspace claim, `with_sandbox`, torn down even after a panic) and `cat` as the "server". `src/lsp/manager.rs`'s run the real rust-analyzer, pyright and gopls (`go install`), configured from the catalog's own suggestion for the mason fixtures, so they pull `rust:1`, `node:22-slim` and `golang:1` and download the servers: they need the network, from inside the cluster (proxy.golang.org, github.com). A DNS or network blip there fails them with errors like `Could not resolve host: github.com`; rerun those tests alone before suspecting the change (SME-86's gate hit this once).
- A fresh server answers before it has indexed: rust-analyzer returns partial references and `-32801 content modified` for a while. Ask until the answer has what you expect (`until_contains`) rather than asserting on the first one.
- A server's out-of-memory kill takes its whole container (cgroup v2 kills the group), so the pod stops as `OOMKilled`; it isn't just the one process.

## Testing the Anthropic streaming client without the network

`anthropic::stream::stream_anthropic_message(endpoint, request, on_delta) -> Result<StreamedTurn, String>` is tested against a mock upstream — a throwaway Axum server bound to an ephemeral port, passed in as the `Endpoint`. The helper `run_against_mock_upstream` does the setup:

```rust
async fn run_against_mock_upstream(
    mock_body: &'static str,
    on_delta: impl FnMut(&str),
) -> Result<StreamedTurn, String> {
    // bind 127.0.0.1:0, serve POST /v1/messages with `mock_body` as text/event-stream
    stream_anthropic_message(&test_endpoint(addr), &request, on_delta).await
}
```

Nothing process-global is involved, so these tests run in parallel. `run_against_responses` serves a list of statuses and bodies for the retry tests; tests of `send_and_await_response` (auth headers, a timeout, an unreachable endpoint) build their own `Endpoint`. The pure parsing logic (`interpret_stream_event`, `block_index`, the `Blocks` accumulator) is tested synchronously. llama.cpp's overlapping-block reply is `src/anthropic/fixtures/llama_cpp_messages_stream.sse`, built from llama.cpp's serializer (see the fixtures' `README.md`); `turn`'s `test_commentary_before_a_tool_call_on_llama_cpp_is_saved` runs a whole turn on it.

**A turn's mock is a provider in the test's own database** (SME-72). `turn`'s test helpers (`start_mock_upstream`, `start_mock_upstream_failing_n_times`, `start_recording_mock_upstream`, `start_hanging_mock_upstream`, `start_partial_then_hanging_mock_upstream`) take the pool and call `providers::test_support::add_mock_provider(pool, addr)`: it saves a provider at the mock's address, makes `MOCK_MODEL` on it the default, and moves every conversation already in that database onto it, so a test that starts a second mock switches to it. A test database with no provider fails any turn at once with `NO_MODEL_CONFIGURED`, without touching the network; the tests in `anthropic::tools` and `sandbox` whose commands wake the model rely on exactly that, where they used to point `ANTHROPIC_BASE_URL` at a dead port. Before SME-72, a real `ANTHROPIC_API_KEY` in the environment could send those wake-ups to the live API.

**Turn tests still take a lock**, `providers::test_support::lock_turn_tests()`, used by `turn`'s tests and by `sandbox`'s Docker-restart notice test: they share process-wide state keyed by conversation id (the turn lock, the reply so far, a stop or pause), and every `#[sqlx::test]` database numbers conversations from 1. Other tests that run turns use `db::create_conversation_with_id` with an id of their own instead (not for a test that creates a sandbox pod: a conversation's `/workspace` claim is named after its id, so a fixed id reuses a claim across runs). The pod lifecycle tests, whose finished commands and crash notices wake the model, move all their ids clear with `start_ids_clear_of_other_runs` instead (SME-94).

A mock upstream that needs to return a *different* response per call (e.g. a tool-use turn, then a follow-up turn once the tool result comes back) tracks a request count with a shared `AtomicUsize` in the route closure and indexes into a `Vec<String>` of bodies, clamped to the last one once exhausted — see `start_mock_upstream`. `start_mock_upstream_failing_n_times` answers the first N requests with an HTTP 500 (Ollama's "error parsing tool call" body) and then a normal stream, for the retry path.

**Model listing** (`anthropic::models`) is tested against the responses in `src/anthropic/fixtures/`: a real llama.cpp `/v1/models`, and Anthropic's and Ollama's documented examples (see its `README.md`), plus mock servers for paging, Ollama's `/api/show`-then-`/api/ps` fallback, a 404 and an unreachable address.

## Testing code that touches a process-global resource across `#[tokio::test]` runtimes

**General hazard, not just `PgPool`:** each `#[tokio::test]` fn gets its own independent tokio runtime. Any process-global resource (a `OnceLock`/`OnceCell`-held value) whose correctness depends on a background task that outlives a single call — a connection driven by a spawned task, a handler loop, anything with a "keep this running or the resource stops working" shape — breaks the same way once more than one `#[tokio::test]` fn touches it: whichever test's runtime first initialized it also owns that background task, and once *that* runtime tears down (at the end of *that* test fn), the resource silently stops working for every other test still trying to reuse it, from a different runtime. Seen twice now — `PgPool` below, and `chromiumoxide::Browser` (`src/webfetch.rs`'s own real-browser test, which hit "send failed because receiver is gone" the first time it split its scenarios into three separate `#[tokio::test]` fns instead of one) — check for this before adding a third. The fix is the same shape both times: either thread the resource through explicitly so each test gets its own runtime-local instance (`PgPool`'s fix), or consolidate every scenario that needs to share one instance into a single `#[tokio::test]` fn (`chromiumoxide::Browser`'s fix, matching `src/browser_tests.rs`'s own already-established "deliberately one test, not several" pattern). A third shape, and the one the shared browser now uses: run the resource's background tasks on a runtime of its own that lives as long as the process (`webfetch::BROWSER_RUNTIME`). One test per file stopped being enough once the app's browser tier (`browser_tests.rs`) also opened a browsing session: the first of the two tests to touch the shared browser took it down for the other (SME-40). A third instance, on SME-42: a real-cluster test that set `sandbox`'s process-global `MANAGER` made `test_terminal_lifecycle_end_to_end` fail with `Kube(Service(Closed))` whenever it ran after that test finished. The kube client's worker lived on the first test's runtime. No unit test sets `MANAGER` now; the browser tier's one test sets it through `sandbox::init()`. A test that calls the sandbox's free functions (`create_pod`, `create_terminal`, ...), which reach the manager through `get()`, calls `use_test_manager(client)` first (SME-94): under `#[cfg(test)]`, `get()` returns a thread-local manager when one is set. That works because `#[sqlx::test]` runs each test on a thread of its own, on a current-thread runtime that runs every task the test spawns on that thread too, so the manager, its client and its tasks share one runtime. A process-lifetime runtime for the client (the browser's shape) doesn't fit kube's: its hyper client spawns each pooled connection on whichever runtime opened it, so parallel tests would share connections that die with another test's runtime. Other real-cluster tests use their own client and `SandboxManager` directly. That's why the public `sandbox::open_pod_target(pool, conversation_id, host, port)` is a thin wrapper over a private `open_pod_port_with` that takes the client: `test_open_pod_port_reaches_the_conversations_own_pod` passes its own.

Most server logic takes `pool: &PgPool` explicitly and uses `#[sqlx::test]`, per "Database tests" above. `turn::run_turn` is the one exception worth calling out: it was *changed* to take `pool: &PgPool` (rather than reaching for `db::get()` internally, which is what `send_message` itself still does) specifically so its own tests could use `#[sqlx::test]`. The first version reached for `db::get()` directly and initialized it once via a shared `tokio::sync::OnceCell` across tests — it worked in isolation but reliably deadlocked/timed out (`PoolTimedOut`) when multiple such tests ran concurrently, because each `#[tokio::test]` gets its *own* tokio runtime, and a `sqlx::PgPool`'s connections become unusable once the runtime that created them is torn down (which happens as soon as the test that happened to initialize the pool finishes) — a later test reusing the same process-global pool object from a *different* runtime hangs waiting for a connection that will never come back. Threading `pool: &PgPool` through instead sidesteps this: every test gets its own runtime-local, `#[sqlx::test]`-isolated pool, same as everywhere else. Any new server-side function that a background task might call (as a finished command's wake-up calls `run_turn`) should take its pool the same way, for the same reason.

**The same hazard inside one test: dioxus's worker runtimes.** `dioxus-server` runs server functions and SSR on a `LocalPoolHandle`, worker threads with a Tokio runtime each. A pooled database connection, or a cluster client connection, first opened while handling a request is tied to that worker's runtime. In a test that runs the app in-process (`browser_tests.rs`), once the server shuts down those runtimes go with it, and a later query on the test's own runtime can pick up such a connection: it either fails ("A Tokio 1.x context was found, but it is being shutdown") or hangs forever. Whether it does depends on which connection the pool hands out, so it shows up as a flaky failure or a CI job that never finishes. Do all database and cluster checks before shutting the in-process server down (SME-39).

## Testing for deadlocks

When testing a code path that could plausibly deadlock (a lock re-acquired somewhere non-obvious, a channel nobody drains), wrap the call in `tokio::time::timeout(...)` and assert it doesn't elapse — a hung test otherwise just stalls the suite with no useful failure message. The pattern was first needed when a tool pushed a notification via an *awaited* `run_turn` call, which deadlocked against the per-conversation lock the *calling* `run_turn` was already holding; the fix was spawning that push instead.

**A test gate built on a semaphore lets one request through per permit only if each permit is consumed:** `gate.acquire().await?.forget()`. A `let _permit = gate.acquire().await?;` inside a block hands the permit back when the block ends, so one `add_permits(1)` lets every later request through. And a race test that passes only once debug output is added is a sign its ordering isn't the one it claims: on SME-113 a refresh-race test passed only with logging slowing it down, and debugging that pass found this gate bug and a real window the fix can't close.

## Tests that touch per-conversation state

Some state is process-wide and keyed by conversation id: the turn lock, a stop, the pause after a stop, and whether a turn is running (one `ConversationRuntime` per conversation in `turn::state`, SME-52), and the event channels. The locks, stop counters and channels are freed when unused (SME-91), so a test checking one is gone has to drop its own handles first (a `conversation_lock`, an `events::subscribe`). But every `#[sqlx::test]` database numbers conversations from 1, and tests run in parallel, so two tests' "conversation 1" are the same key.
- **A test that stops or pauses a conversation, or asserts on its events,** creates it with `db::create_conversation_with_id(pool, <a unique high id>)`, or calls `db::test_support::start_ids_clear_of_other_runs(pool)` before creating it. On SME-94, `turn`'s `test_a_dropped_event_subscription_stops_listening` counted conversation 1's subscribers and failed whenever a turn test's conversation 1 was subscribed at the same moment. On `pod-management`, before this, one test's stop paused every other test's conversation 1, and five unrelated wake and notice tests failed.
- **In a `#[sqlx::test]`, prefer `start_ids_clear_of_other_runs`** (a fresh base on every call) over a literal id. Where a literal is needed (a test with no pool), take it from the ticket's own block, `9_<ticket>_xxx_xxx` (SME-135's are `9_135_…`), and grep for it before using it, with its underscores removed too (`9100000001` and `9_100_000_001` are one id; on SME-135 a sweep that kept the underscores missed literals written the first way). Some older blocks don't follow the scheme and are taken: SME-72's providers tests use `9_172_…` (so SME-172's block is shared with them), older turn and event tests use `9_000_…`, `9_100_…` and `987_654_…`, and `browsing.rs` uses `900_001`. Every hand-picked id is above the range the check below refuses, so a clash there isn't caught at run time: on SME-135, `test_stopping_a_woken_turn_is_not_reported_as_a_failure` shared `9_100_000_014` with a test whose stop leaves that conversation paused, so when it ran second its wake started no turn and it passed without testing anything.
- **Listening on a shared id fails at once.** Under `#[cfg(test)]`, `events::subscribe`, `subscriber_count` and `has_channel` panic on an id below `TEST_SHARED_IDS_END` (100 000), the range every test database hands out: such a test hears, or counts, other tests' events depending on what runs alongside it. When the check went in, it failed exactly the nine tests SME-135 had found. It doesn't catch two tests picking the same high literal (the case above), nor a test that only publishes on a shared id.
- **Tests that wait for one kind of event** skip the others: turns now publish `TurnState` too.
- **A test that gains an assertion that something happened** (a turn ran) replaces any fixed sleep before the act with a wait for that event. A fixed sleep the old test tolerated becomes a flake once the test checks what happened: on SME-135, `test_stopping_a_woken_turn_is_not_reported_as_a_failure` gained "a turn ran" behind a 300 ms sleep, and failed whenever the stop came before the wake had started its turn; it now waits for `TurnState { running: true }`.
- **The app-wide bus (`events::subscribe_app`) has no key,** so every test running alongside shares it. Never count its subscribers or assert on the first event it delivers: wait for the event wanted and skip the rest, knowing another test's identical event can satisfy the wait. A conversation's stream relays all four app events (`PodsChanged`, `TurnsChanged`, `QuestionsChanged`, `ProvidersChanged`); `turn::tests`' `is_app_relay` names them. To check a subscription goes away, give it a channel of the test's own and count that (`api::pods`' `app_events_response`, SME-135).

## Running tests

```bash
scripts/check.sh                              # the per-commit gate (see below)
cargo test --features server                 # the real (server-gated) tests
cargo test --features server -- --nocapture   # show println! output
cargo test --features server test_name        # a single test by name
cargo test --features server -- --exact sandbox::tests::test_name   # exactly one test
```

A name filter matches every test whose path contains it, and several filters are OR'd, so a filter meant for one unit test can also pick up real-cluster tests (on SME-85, `test_a_` matched a claims test that creates pods). To run one test, name it with `--exact` and its full path.

**A real-cluster test runs only this way, under the cluster lock** (see [development-process.md](development-process.md#rules)): on SME-115 a substring filter run outside the lock picked up a new test whose unfixed form was the bug being fixed, and it marked another run's claims in `smelt-park-test` for deletion.

**A module with both pure and real-cluster tests keeps the real-cluster ones in a `cluster` submodule** (as `sweep::tests::cluster::…` does), so a filter on the pure tests' path can't reach them. On SME-134 the filter `sweep::tests::test_the_sweep_`, meant for the pure tests, also ran four real-cluster sweep tests outside the lock.

**Gate from a detached gate worktree** when the lock queue is long, so writing the next commit goes on while a gate waits, and nothing edits the tree being gated (see [development-process.md](development-process.md#rules)). A worktree has its own `target/`, so the gate's build never shares artefacts with the branch's:

```bash
git worktree add --detach ~/smelt-worktrees/gate-<ticket> <commit>
cd ~/smelt-worktrees/gate-<ticket>
cargo build --features server && cargo test --features server --no-run   # build outside the lock
flock "$(git rev-parse --git-common-dir)/smelt-cluster.lock" scripts/check.sh > <scratchpad>/sme-N/gate-<commit>.log 2>&1; rc=$?   # act on rc, not on the log
git checkout --detach <next commit>   # the next gate, once this one is back
```

Name the commit by hash (`git rev-parse --short <branch>`, run anywhere), never `HEAD`: in the gate worktree `HEAD` is its own last gated commit, not the branch's tip (SME-135).

Commit on top of a commit only once its gate passed. Run the browser tier from the gate worktree too. Remove the gate worktree (`git worktree remove`) when the branch merges, with its other worktrees.

Most logic lives behind the `server` feature; plain `cargo test` compiles but skips it.

`scripts/check.sh` is what every commit is gated on (`scripts/check.sh && git commit ...`, see [development-process.md](development-process.md#rules)): the web build (`cargo check` for `wasm32-unknown-unknown`), the server binary build, and the server tests. It fails on any failure, including a warning in either build. It doesn't run the browser tier. The server tests' full log is kept at `target/check/server-tests.log` until the next run.

**The scripts a session runs for the gate and for hands-on checks delete no files** (`check.sh`, `browser-tier`, `check-server`, `build-sandbox-image.sh`): Claude Code's safety check refuses a command whose scripts remove files at paths it can't resolve, and only a person can approve it (SME-107). Logs go to fixed paths under `target/`, overwritten by the next run (`browser-tier`'s `dx build` log is `target/browser-tier/web-bundle.log`), and `check-server` empties its state files (`.check-server.pid`, `.check-server.scratch-db` in the check worktree) rather than removing them: an empty or missing one means nothing is running or there's no scratch database to drop. Dropping scratch databases and stopping processes stay. `scripts/test-check-scripts` checks both, with stubs for `dx`, `sqlx`, `curl` and the image check; run it after changing any of these scripts.

`mcp::tests::test_live_exa_search_through_smelt_mcp_client` checks the built-in Exa MCP server against the real service: smelt's own MCP client connects keylessly, sees only `web_search_exa`, and gets results back. It needs the internet and depends on Exa's unpublished free limits, so it's `#[ignore]`d **and** skips unless `SMELT_LIVE_EXA=1` is set. CI's browser job runs every ignored test, and this one shouldn't depend on Exa there. Run it with `SMELT_LIVE_EXA=1 cargo test --features server live_exa -- --ignored`. See [Definition of done](development-process.md#definition-of-done) for the full two-target check.

## Browser verification

A small automated browser test tier exists (`src/browser_tests.rs`, see below) for behavior that genuinely needs a real DOM to verify — everything else is a hands-on pass, driving a real headless Chrome against a check server.

**Run hands-on checks against `scripts/check-server`, not a `dx serve` in the working tree.** `scripts/check-server start [REF]` builds and serves a commit (default `HEAD`) from a separate worktree (`../smelt-check`, or `$TMPDIR/smelt-check` when the repo's parent isn't writable) on port 8081, and says it's up once the app answers with a 2xx (while `dx` builds it answers with a 500); `scripts/check-server stop` stops it and everything it started; `scripts/check-server status` says what's running. It serves the commit, not your uncommitted edits, and saving a file never restarts it. To check a newer commit, `stop` and `start` again. `CHECK_SCRATCH_DB=1` serves from an empty database of its own (dropped on `stop`), for when the dev database has migrations from another branch. It's refused for a ref without SME-115's fix (one that doesn't contain commit `6c9f03c`; a commit partway through the fix has its migration but still sweeps every claim): every smelt server shares the `smelt-park` namespace, and one from before the fix on an empty database deletes the dev server's sandbox claims (every conversation's `/workspace`). Since the fix, each database labels its pods and claims with its own instance id and touches only those, and a fresh database's ids start above 2,000,000,000, so its names don't meet the dev server's. The same goes for a built `./server` run by hand against a scratch `DATABASE_URL`: only one with the fix. The refusal lives in the script, so run `check-server` only from a branch that contains the fix (`git merge-base --is-ancestor 6c9f03c HEAD`; otherwise merge `origin/main` first): an older branch's own copy has no refusal at all. Before `stop`, delete every conversation the check started a sandbox in, in the check server's own UI: its pods and claims carry the scratch database's instance, and once `stop` drops that database no server deletes them (SME-129). Restart the dev server onto the fix first too: one from before it sweeps a scratch server's claims when it restarts (harmless to your conversations, but it can fail the check). See [development-process.md](development-process.md#rules) for why: a `dx serve` in the working tree rebuilds and restarts on every save, and stopping only the `dx` process leaves its server child running.

**Under `dx serve` (a check server included), every open tab reloads itself when the server restarts:** `dx`'s dev client in the page watches its own socket (`/_dioxus`) and reloads on reconnect. A check that needs a tab to outlive a server restart, such as SME-43's reload banner, has to run built servers instead (`dx build --platform web`, then the bundle's own `./server`); see that ticket's Feature checklist row.

A related gotcha with a `dx serve` that is watching files: **a CSS/asset edit doesn't reliably reach a *fresh* page load.** `App`'s `asset!("/assets/chat.css")` resolves to a content-hashed bundle path baked into the served HTML at build time; `dx`'s hot-reload patches tabs that were already open, but a brand-new browser (exactly what a screenshot script launches each run) can get a stale pre-edit bundle. On `sandbox-native-environment` a fresh `browser_check.py` run kept rendering unstyled markup after a CSS edit that the log said was hot-reloaded. With the check server, commit and restart it (`scripts/check-server stop`, then `start`) before assuming the change itself is wrong.

### Playwright (preferred)

The dev container image bakes in a Python Playwright install specifically so this doesn't have to be rebuilt or asked for per session — see the `Dockerfile`'s `/opt/playwright-venv` stage:

```bash
scripts/check-server start                         # the app, on http://localhost:8081

/opt/playwright-venv/bin/playwright install chromium   # once per container instance —
                                                         # the venv exists in the image,
                                                         # but the browser binary itself
                                                         # downloads into ~/.cache/ms-playwright
                                                         # on first use

/opt/playwright-venv/bin/python your_script.py      # a short sync_playwright() script:
                                                     # launch chromium(args=["--no-sandbox"]),
                                                     # goto/click/fill, .screenshot(path=...)

scripts/check-server stop                          # when done
```

Start from `scripts/ui-check/smelt_ui.py`: a `Tab` wrapper that starts and deletes conversations, sends a message and waits for the turn to finish, reads the transcript and notices, and handles several things that each cost a rerun on SME-51. For example, a conversation page never reaches "network idle" (its event stream stays open), and the message box is an `<input>`. Its docstring has a complete example. It talks to `SMELT_UI_BASE` (default `http://localhost:8081`, the check server) and writes screenshots to `SMELT_UI_OUT` (default `./ui-check-out`).

**A check cleans up after itself.** `run` deletes every conversation its tabs started with `new_conversation`, in a `finally`, so a check that fails or times out still removes them. This matters on a scratch database: a conversation's sandbox pod and claims carry that database's instance label, and once `check-server stop` drops the database no server ever deletes them. On SME-126 a check script died at its own 300-second wait, before its delete, and left a sandbox behind in `smelt-park`. Two habits keep this from happening in the first place:
- **Ask for something that doesn't start a sandbox** unless the check is about one. On SME-126 a request to write a file started a sandbox; a long `todowrite` call tested the same thing without one.
- **Give a local model time.** A turn on the user's llama-server can take many minutes; `wait_idle` waits 15 by default. Don't cut it to 5.

Then view the screenshot (the `Read` tool renders images directly). This is a plain Python script per check, not a fixed CLI (navigating to a conversation, clicking a sidebar entry, reading back `scrollTop`/`scrollHeight` via `page.eval_on_selector`, etc.).

**Before/after comparisons serve each ref from its own check worktree** (`CHECK_WORKTREE=../smelt-check-main scripts/check-server start main`, and another for the branch), not one worktree stopped and restarted on the other ref: `dx` can hand the second run the first one's bundle. Reproduce any difference once before investigating it; a race in a mock (a notice landing mid-turn in one run and after it in the other) looks like a regression too. On SME-57 both happened, and each cost a rerun. For a change to the page's state, the scripted session also switches conversation in the page and back (a sidebar click, not a reload, so the in-page reset runs), and the check server's log is searched for API errors on both refs (SME-57's PR (b)).

### `scripts/browser-check/` (fallback)

Before Playwright was added to the image, UI verification in this sandbox had no browser, no Node, and no Python `pip` available at all (see SME-5's and SME-8's retrospectives) — `scripts/browser-check/` is a from-scratch, pure-stdlib driver built to cover that gap, and is kept as the fallback for an environment that still lacks Docker-rebuild/root access:

```bash
scripts/browser-check/setup.sh                     # once — downloads a headless
                                                     # Chrome-for-Testing binary and
                                                     # its shared libraries into
                                                     # .browser-check-cache/ (gitignored,
                                                     # never committed); idempotent,
                                                     # safe to re-run, no root needed

python3 scripts/browser-check/browser_check.py \
    http://127.0.0.1:8081/ \
    --screenshot /tmp/out.png \
    --action "click:.conversation-item" \
    --action "sleep:1000" \
    --action "scroll:.messages"
```

`scripts/browser-check/cdp.py` hand-rolls just enough raw WebSocket framing (RFC6455) to speak the Chrome DevTools Protocol directly, and `setup.sh` fetches Chrome for Testing plus its missing shared libraries (nss, atk, dbus, X11, mesa, ...) via non-root `apt-get --print-uris` + `dpkg-deb -x` into a local prefix — no root, no system package state touched. `--action` runs steps in order: `click:SELECTOR`, `type:SELECTOR=TEXT`, `wait:SELECTOR` (poll up to 10s), `scroll:SELECTOR` (scrolls to bottom), `sleep:MS`, `eval:JS` (escape hatch — also handy for injecting synthetic markup to preview CSS for a state you don't have live data for, e.g. an error variant when nothing's currently failing). Each run launches its own Chrome and kills it on exit unless `--keep-open` is passed, so repeated runs don't leak orphaned Chrome processes.

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

scripts/browser-tier   # dx build, then the tests below against a scratch database
# which is:
cargo test --features "server browser-test" -- --ignored --test-threads=1
```

`#[ignore]`d by default (needs the two setup steps above, plus a real Postgres and k3s cluster reachable the same way every other real-cluster test already assumes). Run it with `scripts/browser-tier`, which gives it a database of its own: the test runs smelt's migrations on whatever `DATABASE_URL` names, and a branch's migration applied to the shared dev database stops `main` from starting there (SME-72's retrospective). Its ids start at a random base above 2,000,000,000 (`db::test_support::start_ids_clear_of_other_runs`), not at 1: pods and claims are named after conversation, pod and volume ids, and it shares the `smelt-park-test` namespace with the real-cluster unit tests, so a pod an earlier run was still stopping would otherwise hold up its first sandbox (SME-99; `sandbox::tests::test_a_stopping_pod_from_another_run_doesnt_hold_up_the_tier` holds such a pod in Terminating to check it). It's deliberately just the one test for this file's own scope — see SME-10 for the design and reasoning (in-process server via a factored-out `build_router()`, `chromiumoxide` talking directly to `chrome-headless-shell` over CDP rather than a `chromedriver`/WebDriver setup this environment doesn't have). Reaches into `db`/`sandbox`/`anthropic::tools` directly to set up most scenarios (this tier verifies the browser/live-event pipeline, not tool-selection behavior) and asserts against the rendered DOM via `page.evaluate(...)`, not screenshots. Neither this environment nor CI has real Anthropic credentials, so where a scenario needs the model — a send typed into the page (scenarios 8, 13, 14) or a turn the AGENTS.md trust decision wakes — the model is a slow local mock upstream (`MockUpstream`), saved as a provider of the test's own that every conversation it creates uses, so the providers and default of the database it runs against are left alone; it's deleted with the conversations at the end. It runs in CI (`.github/workflows/ci.yml`, see [development-process.md](development-process.md#definition-of-done)). There the web bundle is built by `scripts/ci-web-bundle`: dx sometimes hangs in its server half after the client bundle is written, and the script then prints a warning with every process's state, stops dx and carries on, since the tier needs only the client bundle (SME-67). `scripts/test-ci-web-bundle` checks it against fake dx commands.

Each scenario is an `async fn scenario_*` with its own conversations and tabs, run by `run_scenario` under its own time limit and `catch_unwind` (SME-59). A failing or hung scenario ends alone, its tabs are closed whatever happened (a leftover tab holds one of the browser's six connections per host and would starve later scenarios), the rest still run, and the test fails at the end listing every scenario that failed; each one also prints `browser tier: <name> ok` or `FAILED: <why>` as it finishes. To run some of them, name them: `SMELT_BROWSER_SCENARIOS=stop_a_turn,pods scripts/browser-tier` (an unknown name fails the run). A new scenario is a new function and one `run_scenario` line; it creates what it needs through `Scenario::conversation` and `Scenario::tab` rather than reusing another scenario's. A negative check ("this must not appear") waits on server state, not a fixed sleep: the mock upstream counts the chunks it has sent and the replies still open (`MockUpstream::wait_until_idle`), so a scenario can wait until a reply has finished or been cut off upstream.

It runs these scenarios, in order, each its own `scenario_*` function, in one `#[tokio::test]` (`test_end_to_end_browser_scenarios`). The numbers are only this list's; the code and `SMELT_BROWSER_SCENARIOS` go by name:
1. The sandbox panel on a cold load: one pod, two terminals. Also checks the stylesheet loads.
2. A terminal command's output streaming live, with no reload.
3. `terminate_terminal` removing exactly the right card, and a terminal terminated while the tab is reconnecting gone once it has (SME-43).
4. A reload mid-command rebuilding the panel, with live updates resuming.
5. The context-usage indicator and its detail view (SME-18).
6. A compaction divider, collapsed by default, expanding to the summary.
7. The todo panel: cold load, then a live full replace through the real `todowrite` tool (SME-20).
8. A reply streaming into one conversation stays there when the viewer switches mid-stream; the sidebar picks up the new title.
9. A reply the tab didn't send (a background notification, another tab's send) arriving live.
10. A missing conversation saying so.
11. A background-notification error clearing on a switch.
12. A pod's sidebar dot appearing live in another tab, the Sandboxes page listing it, and Stop there (a two-step button that mustn't move when armed) removing the row and the dot.
13. Stopping a turn, with Stop shown in a second tab that didn't send, and "Stopped." afterwards.
14. Replies streaming to every tab, a reload mid-reply keeping the text so far, the sender seeing its message once, and five tabs leaving room for a Stop.
15. With a browsing session, a todo list and a terminal open, the chat staying usable at laptop width (SME-40 F2, SME-41 D16).
16. Every settings page reachable from the sidebar, and the sidebar's Delete keeping its size when armed.
17. A phone-width window, including the viewport meta tag (SME-40 F8).
18. A URL that isn't a page saying so (SME-40 F10).
19. A tool call as one compact line with its result folded in; a failed one open (SME-41 D2).
20. Dark mode following the system setting (SME-41 D5).
21. One primary action per form, and intro text lined up with its heading (SME-41 D6).
22. A new conversation's intro and example asks (SME-41 D12).
23. A sandbox dev server end to end (SME-42): a server bound to `127.0.0.1` in a real pod loads in the model's browsing session at `localhost`, the model's `sandbox_preview_url` link appears live in the sandbox panel, opens the same page in a tab through the harness's own preview listener (`SMELT_PREVIEW_URL` set to a free port), and survives a reload. Since SME-33 it goes on to a Docker container with an unpublished port, on a network with a fixed address: `webfetch` and a browsing session load it at its address, a private address outside the Docker range stays refused, and its preview link names the container in the panel and opens in a tab. A container page's own script loads and one fetched with no referrer is refused (SME-90).
24. The model picker naming the conversation's model, and a choice in one tab showing in every tab with no reload (SME-72).
25. An OAuth MCP server taking extra headers (SME-76).
26. A tab older than the server (SME-43): an event type the bundle doesn't know (`ConversationEvent::BrowserTestAddedLater`, which only exists with `browser-test`, so the `dx build` bundle really lacks it) neither ends the stream nor loses the next event, and asks for a reload; a reconnect to a server with another build id (`api::version::test_override`) does too, on the chat and Sandboxes pages; Reload clears it.
27. A small scroll up in the transcript staying where it was put (SME-83).
28. Switching away from a conversation and back leaving its sandbox terminal following its last line, never painted at its top — unhurried, racing the rebuild, mid-burst, rapid repeated switches, a reload with completed output, and the same switch-back at phone width through the drawer; plus a window resize landing on the follow mid-stream, a user scroll-up mid-stream, and a window grow clamping a reader back onto the bottom, which the sticky logic must tell apart (SME-108).

Then, unnumbered: a repo's AGENTS.md waiting for the user's trust, and trusting it loading exactly that file (SME-32); and switching conversations closing the context detail view (SME-51 B11).

Lessons from writing these:
- **Keep the number of open smelt tabs low: close tabs a scenario is done with.** Over HTTP/1.1 the browser allows 6 connections per host across all tabs. Each chat tab holds one always-open stream, and a tab's own loading needs a free connection besides its stream (for its snapshot requests). So about five smelt tabs is the most the harness can have open at once: past that, a new tab never finishes loading. Both this and a Stop click queuing behind a reply's own stream (fixed on `connection-limits`, when replies moved onto the conversation stream) were hit while writing scenarios 12–14.
- **Before typing into or clicking the page, wait for the WASM client to be live** (`wait_for_live_client`, or `wait_for_resource` on a page without a conversation). The server-rendered page accepts typing before hydration with no handlers attached, so input is silently lost; the harness page also permanently shows `dx`'s "Your app is being rebuilt" overlay, which is a red herring. The signal is the page's own: once its live stream is connected and the snapshot pull after it has finished, the chat panel carries `data-live="<conversation id>"` and `data-live-pulls="<n>"` (how many times this page has connected and pulled, so a reconnect shows as a second pull). Neither is set while the stream is down. Before SME-59 the signal was the last snapshot request showing up in the page's resource timings, which fixed the order of the pull and would have stopped working once the page's 250-entry resource-timing buffer filled. On a page without a conversation, wait for a request only the hydrated page makes (the server render never sends it), then read a typed field back before relying on it. On SME-111 a scenario typed into the new-provider form before hydration and the address came back empty; waiting for `/api/price-sources` fixed it.
- **Click through `click_when_present`, which waits for a still target.** A chromiumoxide click reads the element's position and then presses the mouse there, in separate round trips, and the chat page re-scrolls its transcript to the bottom as data arrives after a load. A click aimed at an element that moves in between lands on something else: on SME-68 that was 8 clicks in 40 on the compaction divider with the CPU throttled, pressing `.messages` instead. `click_when_present` waits until the element's box is unchanged across two reads 150 ms apart and, when on screen, it's under its own centre (`wait_for_stable`, which scrolls nothing itself: scrolling the transcript would turn off its stick-to-bottom). Then it clicks and checks the `click` event's target was that exact element (the event a `<details>` toggles on; the press and release are separate round trips, so the press alone proves nothing), trying again if not. A click that loads another page counts as landed. Don't call chromiumoxide's `.click()` directly. For a toggle, assert on its state (a `<details>`'s `open`) as well as its content, so a failure says whether the click missed.
- **The harness cleans up after itself, even when a scenario fails.** It runs against the real dev database and cluster, so every conversation a scenario creates is recorded (`new_conversation`) and removed at the end, pods first, the same way deleting a conversation in the app does. The test then checks nothing was left. That cleanup has to run *before* `harness.shutdown()`: once the harness has shut down, the cluster client's connection is gone ("runtime dropped the dispatch task") and pod deletes silently fail. Leftover pods matter: enough of them and new pods stop starting (`create_pod: Timeout`, then `ProtocolSwitch(500)` on terminals). Other tests can still leave some behind (a crashed run, `sandbox`'s OOM tests); the lifecycle harness's sweep deletes them at the first gate after they're an hour old (see [the pod lifecycle tests](#the-pod-lifecycle-tests-sme-94)). `scripts/clean-test-namespace.sh` clears every pod and Docker data claim at once, a live run's included, so run it only when no test run is going.

**The page is styled only because the harness serves the stylesheet itself.** A plain `cargo test` build doesn't bundle assets: `asset!("/assets/chat.css")` resolves to the source file's own path (`/app/assets/chat.css`), which neither the bundle nor dioxus-server serves, so until SME-40 every page in this tier ran unstyled and any layout measurement there was meaningless. The harness now routes that exact URL to `assets/chat.css` (and `assets/highlight.css`'s, SME-30), and scenario 1 asserts the stylesheet loads. A new asset referenced with `asset!()` needs the same treatment before a scenario can rely on it: add it to the harness's stylesheet list.

`src/webfetch.rs` has its own separate `#[ignore]`d, `browser-test`-gated real-browser test (`webfetch::browser_tests::test_fetch_scenarios`) — same `chrome-headless-shell` binary/setup, same `cargo test --features "server browser-test" -- --ignored --test-threads=1` invocation runs both, but a different module/concern (a feature's own browser-driving + SSRF-guard logic, not DOM/panel rendering), so it isn't a scenario folded into `browser_tests.rs`'s one test. `src/browsing.rs`'s real-browser scenarios run inside it too, as a plain async fn it calls (`browsing::browser_tests::run_session_scenarios`), since they share the same browser. Also different in one real way: it uses `webfetch`'s real (production) `shared_browser`, not `browser_tests.rs`'s own harness — and, having hit the cross-runtime hazard above first-hand, keeps all its scenarios in that one `#[tokio::test]` function. `webfetch.rs` has two more ignored tests, for Chrome not outliving its owner: `test_chrome_exits_with_its_owning_process` and the `chrome_owner_helper` it re-runs as a child (see below).

**A piped `cargo test` invocation can look hung when it's actually finished.** If a test spawns a long-lived child process (a shared browser, kept running by design rather than torn down per-test), that child inherits and can hold open any stdio the parent process didn't explicitly close or redirect — `chromiumoxide` only pipes `chrome-headless-shell`'s stderr, not its stdout, so the browser keeps the test binary's own stdout fd alive for as long as it runs. Piping `cargo test`'s output through another command that waits for real EOF (`| tail`, `| grep`, ...) then blocks forever, even after `cargo test` itself (and the Rust test process) have already cleanly exited — not a code bug, a shell-pipeline artifact. Redirect to a real file (`> output.log 2>&1`) instead when a test launches anything long-lived; a file read doesn't block waiting for every writer to close.

**A performance scenario measures a baseline on the old code first,** and asserts a ratio rather than an absolute time: the script time Chrome reports in its page metrics for a small case against a large one (SME-57's PR (c)). An absolute limit passes or fails with the machine and the cluster's load. `scripts/browser-tier` hides a passing test's prints; set `RUST_TEST_NOCAPTURE=1` to see the numbers a scenario logs.

## What's not covered yet

- **The automated browser tier covers what its scenarios list, not every page.** It's not a general framework other features are expected to plug into; add a scenario when a change needs real-DOM verification that should keep running. `webfetch.rs`'s separate test covers real navigation, SSRF-guard behavior, and the browsing-session tools (below).
- **The live browsing panel's input and frame wiring has no automated coverage** — only a manual pass with a real app and Playwright, driving a real model through a real conversation, confirmed it renders live frames and forwards input correctly. Browser scenario 15 checks the panel's layout, nothing more. The *tools* underneath it (`browsing.rs`'s own session/navigate/click/fill/screencast/input-dispatch logic) are automated-tested inside `webfetch.rs`'s real-browser test, and the panel's pure frontend logic (`chat/panels/browsing.rs`'s `browser_input_event_for_key`, `cdp_modifiers`, `frame_point`, `wheel_delta_pixels`, `coalesce_mouse_moves`, `address_bar_value`) has direct unit tests. The real-browser scenarios include regressions for what the branch's review found: a plain hover reaching the page with `buttons == 0`, Enter submitting a form, a viewer leaving and another arriving with no gap in frames, a subscription from a closed session not affecting the next one, the frame stream releasing its viewer when dropped, two racing `open_session` calls where exactly one wins, a `data:` URL being refused, a typed password never appearing in the element list, `fill` replacing (and clearing) a field instead of appending, Shift+Tab and Ctrl+Backspace keeping their modifiers, a second viewer getting a frame from a static page, and a close issued mid-open winning. Also: a click waiting for a delayed update and for a slow page it navigates to; `fill` with accented, CJK, emoji and multi-line text; and nothing a page does — a popup, a `target=_blank` link, a WebSocket, a service worker — reaching a refused address, in both `browsing` and `webfetch`. That last check needs an address the guard refuses but that is still reachable, and the tests' guard allows loopback. So it listens on this machine's own private address instead (found by opening a UDP socket toward a private range, which sends nothing) and fails loudly if the machine has none. The address bar's URL tracking is covered too: a URL update for the model navigating, a link click, a `pushState` change, and a refused load (which must report the address asked for, not `chrome-error://`). Browser-data isolation is covered from both sides: a cookie and a localStorage value set in one conversation's session must be visible to that session (so the check can't pass vacuously), invisible to another conversation's, and gone after a close and reopen. A second `webfetch` call must not see what a first one set. `webfetch::browser_tests::test_chrome_exits_with_its_owning_process` checks that Chrome can't be orphaned. It re-runs the test binary as a child process, which then owns a shared browser (via the `chrome_owner_helper` test, a no-op unless `SMELT_CHROME_OWNER_HELPER` is set). The test finds that child's Chrome through `/proc`, SIGKILLs the child and asserts Chrome exits too. The static-page scenario navigates somewhere with no focused input on purpose: a blinking caret keeps Chrome sending frames, which hid this bug from the first version of the test. What's still manual-only is the actual RSX event wiring (mouse/wheel handlers reading `element_coordinates()` and scaling through `frame_point`, the frame `<img>` reactively updating off the live stream), since automating "one headless browser watching another headless browser's live video feed and clicking on it" is a meaningfully bigger lift than `browser_tests.rs`'s existing scenarios. Worth a real automated scenario later, not assumed away.
- **No native SSR component-test harness.** Components aren't unit-tested by rendering them to a string outside a real page load. Worth adding if/when component logic grows complex enough that manual browser verification alone becomes slow to iterate on.
