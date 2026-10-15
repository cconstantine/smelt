# Sandbox tests

Part of [testing.md](../testing.md). The real-cluster tests: pods, Docker, the agent's protocol and language servers.

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

**To hold a pod at a start-up stage on purpose, block that stage deterministically rather than starving the node.** `sandbox::tests::cluster::test_a_pod_stuck_before_its_containers_says_what_it_waits_on` (SME-132) gives a pod a volume naming a ConfigMap that doesn't exist: the kubelet can't mount it, so the pod stays at `PodReadyToStartContainers: False` with `FailedMount` events every time, which is where SME-132's browser-tier flake timed out, and no container ever starts, so it costs the node nothing. The kubelet mounts only the volumes a container uses, so the volume needs a mount too. The pod has `activeDeadlineSeconds: 60`, so a run killed after the create leaves a pod the kubelet fails with `DeadlineExceeded`. It checks the deadline only once its 2-minute wait for the mount gives up: a killed run's pod failed at 127 s, not 60. Loading the node to reach a stage slows every other gate and the user's dev pods on the one shared node, and reaches the stage only some of the time.

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

## The pod lifecycle tests (SME-94)

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

**A real-cluster test that judges an object's age compares its `creationTimestamp` with another time from the API server** (such as another object's `creationTimestamp`), never the runner's clock. Kubernetes cuts `creationTimestamp` to the whole second, so against the runner's clock an object can read as up to a second older than it is (and as younger when the runner's clock is behind the API server's). On SME-143 the sweep's made-between-listings test swept with a 1 s age limit against `Timestamp::now()`; on CI's slower runners its young pod read as old once the next whole second passed, and was deleted with its claim, in 3 of 7 runs. `sweep_with` takes the time to judge age at for this reason: that test passes its claim's `creationTimestamp` plus 1 s.

**A test-side wait for something smelt bounds by an env-configurable timeout is computed from that timeout,** never a literal sized from the local default: CI raises some of them (`SANDBOX_RUNNING_WAIT_TIMEOUT_SECS=120` in `ci.yml`). Teardown's wait for a pod start is `2 * running_wait_timeout()` plus a minute for this reason; on SME-94 a literal 150 s, sized from the local default, was shorter than a start can take in CI.

## Docker in the sandbox (SME-33)

- `test_docker_in_a_sandbox_pod_works_and_stays_inside_the_pod` and `test_an_oom_in_a_nested_container_restarts_only_the_docker_sidecar` run real containers. Their base image is the sandbox's own files (`sudo tar -C / -c bin sbin lib lib64 usr etc | docker import - local/base`), so no test pulls from Docker Hub. Leave out `usr/lib64` or `etc` and a container fails with `exec /usr/bin/sh: no such file or directory`.
- An OOM test needs memory that's actually written: `head -c 900M /dev/zero | tail` holds it, Python's `bytearray(n)` doesn't (its zero pages are never allocated, so nothing is killed). After the sidecar's OOM kill, a restarted dockerd reports the old containers as `Exited (255)`, often before the pod's status shows the restart.
- Each test's checks run inside `catch_unwind`, so the pods and claim it made are deleted even when an assertion fails; a panic otherwise unwinds past the cleanup.
- Run `scripts/build-sandbox-image.sh` after changing the agent (`src/bin/sandbox_agent.rs`, `src/agent_protocol.rs`, `src/docker_net.rs`), its dependencies (`Cargo.toml`, `Cargo.lock`) or `docker/sandbox/`: the pod runs the imported image, not the tree you just built. The image is named after the agent sources it was built from, `smelt-sandbox:src-<first 16 hex of scripts/agent-sources-hash>` (`scripts/sandbox-image-ref`), and `scripts/check.sh`, `scripts/browser-tier`, `scripts/check-server` and CI run their pods from it through `SANDBOX_IMAGE` (SME-102). So a branch that changes the agent builds and tests its own image while the image the dev server runs, named after the main checkout's sources, stays as it is; a fresh worktree with unchanged agent sources needs no build, because the image named after those sources is already in the cluster, whoever built it (SME-84). A server with no `SANDBOX_IMAGE` runs the image named after the sources it was built from (`build.rs` computes the same name, and `sandbox::tests::test_with_no_setting_the_image_is_the_one_named_after_this_trees_agent_sources` keeps it equal to the script's; SME-121), so the gate that merges an agent change has usually already built the image the dev server will want. `--latest` also moves `smelt-sandbox:latest`, which nothing uses by default; a persona never passes it. `scripts/cluster-doctor` (run by `check.sh` and `browser-tier`) says when the image is missing: it checks, through `sandbox_image_import --check <references>` (the read-only `ctr images ls -q`), that the node has the image the tests will use and the Docker sidecar's. The kubelet deletes images no pod is using once the node's disk passes 85%, and every pod then fails with `ErrImageNeverPull`; old `src-<hash>` images go the same way.

## The sandbox agent's protocol (SME-53)

- Neither end needs a cluster to be tested. `sandbox_agent`'s `socket_tests` run its real router on a loopback port with real `bash` terminals and a tungstenite client. `sandbox::agent_connection_tests` put a fake agent on a loopback port behind `dialer_for` (keyed by pod id, so parallel tests don't meet) and script its hello and replies, for smelt's side: timeouts, a dropped connection, outdated agents, two callers connecting at once, a teardown during a connect. They use `#[sqlx::test]` for the pod's row; `pod_row` moves the pod id sequence so the registry, which is keyed by pod id, never sees two tests' pods as one.
- Change `src/agent_protocol.rs` and the fixture test (`src/fixtures/agent_protocol_v1.jsonl`) fails until the change is either additive (bump the minor, add an example) or a new major with a new fixture file. `PROTOCOL_VERSION`'s comment says which is which.
- `pod_lifecycle_tests` are where the real agent runs in a real pod: rebuild the image after changing either end of the protocol, or their pods come up with an agent smelt reports as outdated.
- The agent's `test_an_unparseable_message_is_not_logged_verbatim` captures logs through one process-wide subscriber limited to the agent's own target: a per-thread subscriber misses events whose callsite another test's thread registered first, and tungstenite's trace logs dump every frame the test client sends.

## Language servers (SME-35)

- `src/lsp/pods.rs`'s cluster tests use a stand-in sandbox (the sandbox image, idle, with the conversation's workspace claim, `with_sandbox`, torn down even after a panic) and `cat` as the "server". `src/lsp/manager.rs`'s run the real rust-analyzer, pyright and gopls (`go install`), configured from the catalog's own suggestion for the mason fixtures, so they pull `rust:1`, `node:22-slim` and `golang:1` and download the servers: they need the network, from inside the cluster (proxy.golang.org, github.com). A DNS or network blip there fails them with errors like `Could not resolve host: github.com`; rerun those tests alone before suspecting the change (SME-86's gate hit this once).
- A fresh server answers before it has indexed: rust-analyzer returns partial references and `-32801 content modified` for a while. Ask until the answer has what you expect (`until_contains`) rather than asserting on the first one.
- A server's out-of-memory kill takes its whole container (cgroup v2 kills the group), so the pod stops as `OOMKilled`; it isn't just the one process.
