# K8s sandbox lifecycle

**Branch:** `k8s-sandbox`

## What

A `sandbox` module that can create a disposable Kubernetes Pod in a
`smelt-park` namespace, run a command inside it via `pods/exec`, and delete
it — a proxy the server talks *to*, never a thing the server's own process
acts as. This is the sandboxing piece of `projects/ideas/coding-session.md`,
scoped down to just the lifecycle primitive: create, exec, delete.

Two clusters, two roles: a `k3s` service added to `docker-compose.yml` is
the default target for `cargo test` (hermetic, matches how Postgres already
works — no dependency on a specific network being reachable), and the real
`homelab` cluster (`.kubeconfig.yaml`) is for manual/production
verification only, not something CI or a routine test run depends on.

Explicitly **not** in this plan (deferred to follow-on plans, per the idea
doc's own sequencing — "worth designing and reviewing on its own before
wiring it into the conversation loop"):

- Wiring any tool (`bash`, file read/write) through `anthropic::tools::execute`
  so a real conversation can use it. This plan produces the proxy; the next
  plan points a tool at it.
- Streaming exec output into `ChatEvent`/the browser — the idea doc's
  "Visibility" section flags this as needing its own design. This plan's
  `exec` is buffered (returns stdout/stderr/exit code once the command
  finishes), not incremental.
- `git clone`/checkout inside the pod, or the credential-`Secret` wiring for
  private repos.
- An idle-timeout reaper / GC sweep for orphaned pods (a session that's
  never explicitly deleted currently leaks its pod forever) — same category
  of known gap as the `run_async` task registry's, documented rather than
  solved here.
- `RuntimeClass` (gVisor/Kata) and `NetworkPolicy` — both previously
  decided as cluster-admin/deferred concerns, not smelt code.
- A graceful-shutdown hook that deletes sandboxes on `SIGTERM`/restart —
  decided *against*, not just deferred. See "Restart behavior" under `How`.

## Which files

- `Cargo.toml` — add `kube = "4.2"` (features `client`, `runtime`) and
  `k8s-openapi = "0.28"` (feature `v1_34`, matching both clusters' server
  version — see below) as `server`-only optional deps, same pattern as
  `sqlx`/`reqwest`.
- `src/sandbox.rs` (new, `#[cfg(feature = "server")]`, registered in
  `src/main.rs` next to `mod db`) — the `Sandbox` handle type (`exec` +
  `Drop`) and the `SandboxManager` type (`create`/`delete` plus the
  cleanup channel and its background drain task) — see `How`.
- `src/main.rs` — `mod sandbox;` behind the `server` feature gate, plus
  constructing a `SandboxManager` (which spawns its drain task) at startup,
  next to `db::init()`.
- `.gitignore` — done already (`.kubeconfig.yaml`).
- `docker-compose.yml` — two new services:
  - `k3s`, image `rancher/k3s:v1.34.6-k3s1` (pinned to match `homelab`'s
    actual version, to avoid API-shape drift between the two), `privileged:
    true` (same category of tradeoff already accepted for the `docker`
    dind sidecar), a named volume for `/var/lib/rancher/k3s` state, and a
    second volume — `k3s-config:/etc/rancher/k3s` — shared read-only with
    `smelt`, since that's where k3s writes its generated kubeconfig
    (`k3s.yaml`) on startup.
  - `k3s-bootstrap`, a one-shot service (`restart: "no"`, `depends_on: k3s`
    with a healthcheck-gated condition) that applies
    `k8s/smelt-park-rbac.yaml` against the new cluster using the `kubectl`
    already bundled in the `rancher/k3s` image — creates the `smelt-park`
    namespace, the `park` ServiceAccount, and a Role/RoleBinding
    recreating the same scope already granted on `homelab` (see the
    sandboxing research: `pods` create/delete/exec/log, `secrets`,
    `configmaps`, `services`, `ingresses`, `pvc`; nothing cluster-scoped).
- `k8s/smelt-park-rbac.yaml` (new) — the RBAC manifest above. This becomes
  the actual versioned source of truth for what the `park` service account
  can do; `homelab`'s copy was created by hand and isn't currently defined
  anywhere in the repo — worth applying this same file there by hand once,
  as a manual follow-up, so the two don't drift.
- `src/sandbox.rs` test support — a helper that reads the raw kubeconfig
  from `/etc/rancher/k3s/k3s.yaml` (the shared-volume path) and rewrites
  its `server: https://127.0.0.1:6443` to `https://k3s:6443` before
  building a `kube::Config`/`Client` — k3s writes a kubeconfig assuming
  access from inside its own container, and `127.0.0.1` from `smelt`'s
  container means `smelt` itself, not the `k3s` one. Doing this rewrite in
  versioned Rust rather than a shell step keeps it in one place instead of
  adding a third moving compose part.
- `docs/setup.md` — document `KUBECONFIG` as a `server`-required env var:
  set by `docker-compose.yml` to the shared-volume path by default (so
  `cargo test --features server` works out of the box, matching
  `DATABASE_URL`'s existing pattern), pointed at `.kubeconfig.yaml` only
  when deliberately targeting `homelab`.
- `docs/testing.md` — a new section documenting that `sandbox` tests hit
  the compose-provided `k3s` service by default (mirroring the "Database
  tests" section's real-Postgres pattern — a real dependency, not a mock,
  but a hermetic one), the unique-pod-name-per-test convention standing in
  for `#[sqlx::test]`'s automatic isolation, and how to point tests at
  `homelab` instead for a manual/production check.
- `docs/architecture.md` — add `src/sandbox.rs` to the module map table.

## How

Two types, split by responsibility: **`Sandbox`** is a plain live-pod
handle with one real behavior (`exec`) and passive cleanup; **`SandboxManager`**
owns the shared `kube::Client` and cleanup channel and is where the
async, `Result`-returning lifecycle operations (`create`/`delete`) live.
This is a deliberate split from an earlier draft of this plan, which put
`create`/`delete` directly on `Sandbox` — that made confirming a deletion
(awaiting it, observing its error) and *not* confirming one (just dropping
the value) look like the same kind of operation with an artificial
explicit/fallback distinction between them. Separating the two types makes
them actually different: call `manager.delete(sandbox)` when you want to
know it worked; just drop a `Sandbox` when you don't need to wait — no
special-casing needed on `Sandbox` itself, and no `Option`/disarm dance.

**`Sandbox`** — `{ pod_name: String, client: kube::Client, cleanup_tx: mpsc::UnboundedSender<String> }`:

- `pub async fn exec(&self, command: &[&str]) -> Result<ExecResult, SandboxError>`
  where `ExecResult { stdout: String, stderr: String, exit_code: i32 }` —
  uses `Api<Pod>::exec` with `AttachParams::default().stdout(true).stderr(true)`,
  collects the returned streams to completion, and reads the exit code off
  the resulting `AttachedProcess`. No wall-clock timeout on the command
  itself in this plan (matches "not in scope" above — resource
  limits/timeouts beyond what's here are a known gap, same posture as the
  `run_async` gaps already documented in `state.md`).
- `impl Drop for Sandbox` unconditionally sends `self.pod_name.clone()` on
  `cleanup_tx` — a plain, non-async, non-panicking channel send (a raw
  `tokio::spawn` inside `drop()` isn't safe here: it calls `Handle::current()`
  internally, which panics if the value is dropped outside a live Tokio
  runtime). This is the *normal* way a sandbox gets torn down when nothing
  needs to confirm it — removing an entry from the per-conversation
  registry, for instance — not a fallback for a mistake, so it logs at
  `info!`, not `warn!`, on send.

**`SandboxManager`** — `{ client: kube::Client, cleanup_tx: mpsc::UnboundedSender<String> }`,
constructed once at server startup (next to `db::init()`), which also
spawns the background task that owns `cleanup_tx`'s receiver:

- `pub async fn create(&self, session_id: &str) -> Result<Sandbox, SandboxError>` —
  builds a Pod spec named deterministically from `session_id` (e.g.
  `sandbox-{session_id}`). **Get-or-create, resolved in favor of reuse:** if
  a pod by that name already exists and is `Running`, `create` returns a
  `Sandbox` wrapping it instead of creating a duplicate; only if none
  exists does it apply a new one via `Api<Pod>::create` and poll until
  `Running`. This is both the "one sandbox per conversation" lifecycle rule
  from the idea doc, and — deliberately — what makes an active
  conversation's sandbox survive a smelt server restart with no
  persistence of its own; see "Restart behavior" below. What to do if the
  existing pod is found in some other phase (`Terminating`, `Failed`) is
  still open — see "Open questions." Container image: `busybox:1.36` for
  this plan (already confirmed pullable against this cluster during the
  earlier research spike) — a purpose-built sandbox image with
  git/bash/coreutils is a follow-on concern once a real tool needs more
  than a shell. Container command is `["sleep", "infinity"]` so the pod
  stays alive across multiple `exec` calls over a session's lifetime.
  CPU/memory requests+limits are set on the container spec (the namespace
  has no `LimitRange`, so this is the only enforcement there is — see prior
  sandboxing research).
- `pub async fn delete(&self, sandbox: Sandbox) -> Result<(), SandboxError>` —
  the explicit, confirmable teardown path: awaits `Api<Pod>::delete`
  directly using `sandbox.pod_name`, then `std::mem::forget(sandbox)` so
  `Drop` doesn't also fire and send a now-redundant cleanup message. Safe
  specifically because none of `Sandbox`'s fields have meaningful Drop side
  effects of their own to skip (a `String`, a `kube::Client` which is
  cheaply-`Clone`/`Arc`-backed, an `UnboundedSender` whose `Drop` is just a
  refcount decrement) — worth a comment at the call site saying exactly
  that, since `mem::forget` looks alarming out of context.
- Owns the background drain task: receives pod names off the channel and
  issues the same `Api<Pod>::delete` call, with a timeout and
  `tracing::error!` on failure. This is the path that actually runs for
  every `Sandbox` that's just dropped rather than explicitly deleted
  through the manager.

**Restart behavior.** A `Sandbox` has to be reachable across multiple tool
calls within one conversation, which means it lives in a process-global
registry (a `LazyLock<Mutex<HashMap<ConversationId, Sandbox>>>`, same shape
as `events.rs`'s bus or `anthropic::tools`'s `TASKS`). Rust does not run
`Drop` for values inside `'static` globals when the process exits —
gracefully or via crash — so neither `Sandbox::drop`'s enqueue nor
`SandboxManager::delete` runs on restart, the same way `TASKS` today
silently loses in-flight background tasks on restart (documented in
`state.md`). This is intentional, not an oversight: the k8s Pod itself is a
resource independent of the smelt process, so it keeps running
`sleep infinity` across a restart regardless. Combined with `create`'s
reuse-on-`Running` behavior above, the next tool call after a restart
transparently finds and reattaches to the same pod — no persistence code
needed. This is exactly why a shutdown hook that walks the registry and
calls `manager.delete()` on every sandbox at `SIGTERM` was rejected
outright (see `What`): for an app whose normal dev loop is "restart the
server to pick up a change," that would silently destroy every open
conversation's in-progress sandbox state on every ordinary restart.

**Client construction:** `SandboxManager::new` takes a `kube::Client` built
via `kube::Client::try_default()`, which resolves `Config::infer()` —
respects the standard `KUBECONFIG` env var. In `docker-compose.yml`,
`smelt`'s `KUBECONFIG` points at the shared-volume path written by the
`k3s` service (after the localhost→`k3s` rewrite above); pointed at
`.kubeconfig.yaml` instead only when deliberately targeting `homelab`.

**Testing:** mirrors the existing "real dependency, not a mock" pattern
used for Postgres (`#[sqlx::test]`) rather than the "mock upstream" pattern
used for Anthropic (`anthropic::stream`'s throwaway Axum server) — there's
no natural way to fake the Kubernetes API surface as cheaply as a
hand-rolled SSE responder. Unlike the plan's earlier draft, the dependency
is now hermetic rather than reachability-dependent on a specific home
network: `cargo test --features server` talks to the compose-provided
`k3s` service, the same way it already talks to the compose-provided
`postgres` service. Each test:

- Generates a unique session id (e.g. a random suffix) so concurrent test
  runs don't collide on pod names — standing in for `#[sqlx::test]`'s
  automatic per-test database, since there's no equivalent macro here.
- Calls `manager.create(...)`, asserts the pod reaches `Running`, calls
  `sandbox.exec(...)` with a known command (`echo hello` → assert
  `stdout == "hello\n"`), then calls `manager.delete(sandbox)` and asserts
  `Ok(())` before the test ends — a single deterministic await per step,
  same shape as the plan's earlier draft; no test relies on another test's
  or a reaper's cleanup.
- One additional test covers the Drop path specifically, since it's a
  genuinely different code path from `manager.delete`, not just an
  alternate way of calling the same thing: create a sandbox, `drop(sandbox)`
  without going through the manager, then poll (wrapped in a
  `tokio::time::timeout`, not an unbounded wait) for the pod to disappear
  once the background drain task processes the queued name — proving that
  path actually deletes, not just that it compiles.
- Requires `docker compose up -d k3s k3s-bootstrap` (or the equivalent
  full-stack `docker compose up -d`) to have completed and `KUBECONFIG` to
  be set, same precondition class as `DATABASE_URL`/
  `docker compose up -d postgres` for DB tests; document this in
  `testing.md` and skip/fail clearly (not hang) if unset.
- A separate, non-default manual check (documented, not part of
  `cargo test`) points `KUBECONFIG` at `.kubeconfig.yaml` to confirm the
  same code actually works against `homelab`, catching drift between the
  two clusters' RBAC or version before it surfaces as a production-only
  failure.

This is the plan's "spike the riskiest assumption first" — proving
`kube-rs` can actually create/exec/delete against a real cluster and RBAC
from Rust, not just from `kubectl`, before anything is built on top of it.

## Open questions / tradeoffs

- **Non-`Running` existing pod on `create`:** resolved in favor of reuse
  for the `Running` case (see `How`), specifically for restart-safety. Still
  open: what `create` does if a pod named `sandbox-{session_id}` exists but
  is `Terminating` (a delete still in flight) or `Failed` — wait, error, or
  delete-and-recreate? Not resolved here; the tests in this plan only cover
  the clean-slate and happy-path-reuse cases.
- **Base image:** `busybox:1.36` proves the plumbing but can't run `git` or
  much else. Fine for this plan's scope; the next plan (wiring a real tool)
  will need to decide on a real sandbox image, which is a bigger question
  (build our own vs. reuse an existing dev image) worth its own discussion
  rather than deciding it here.
- **No timeout on `exec` or reaper for `create`'s `Running`-wait** — both
  are unbounded awaits in this plan. Per `development-process.md`'s
  "bound the boundaries" rule this is arguably a gap worth closing even at
  this stage (add a `tokio::time::timeout` around both) rather than
  deferring — flagging for a decision before implementation starts rather
  than after.
- **k8s-openapi's `v1_34` feature** — need to confirm at implementation
  time that `k8s-openapi` 0.28 actually ships a `v1_34` feature flag (it
  tracks upstream k8s minor versions closely but the exact set needs
  checking against the crate's docs, not assumed from this plan).
- **Two clusters drifting apart:** `k8s/smelt-park-rbac.yaml` is the
  versioned source of truth going forward, but `homelab`'s existing RBAC
  was created by hand before this file existed — someone needs to actually
  apply the file there (a manual, one-time step, not something `smelt`'s
  own code or compose can reach out and do) for the two to start in sync.
  Nothing currently detects if they drift apart afterward.
- **State persistence across `docker compose down`:** the `k3s`/
  `k3s-bootstrap` volumes are named (not ephemeral), so cluster state and
  the bootstrapped RBAC persist across restarts like `postgres-data`
  already does. `k3s-bootstrap` needs to be safe to re-run against a
  cluster that already has its namespace/SA/Role (idempotent `kubectl
  apply`, not `create`) rather than erroring on the second `docker compose
  up`.
- **Version bump discipline:** the `rancher/k3s` image tag needs a manual
  bump whenever `homelab` is upgraded, or the two clusters silently drift
  in k8s API version — no automation catches this in this plan.
- **Drain task itself failing:** if the background task that drains the
  cleanup channel panics or the delete it issues fails after retries, the
  pod name is lost (the channel has no persistence) and that pod leaks
  until whatever future idle-timeout reaper sweeps it. Acceptable for this
  plan's scope (same posture as every other gap deferred to that reaper)
  but worth having `tracing::error!` loud enough that it's noticed rather
  than silently swallowed.
