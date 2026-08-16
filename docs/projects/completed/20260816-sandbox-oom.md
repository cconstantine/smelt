# Sandbox pod resource limits + reacting to a pod dying

**Branch:** `sandbox-oom` · **Plan:** `projects/plans/sandbox-oom.md` (removed)

Resumed from an on-hold plan (`SANDBOX_MEMORY_LIMIT`/`CPU_LIMIT` had been
removed outright — see `20260816-file-tools.md`'s retro — as a stopgap
after the original `512Mi` limit proved too small for real work, with no
way to detect or attribute an OOM kill when it happened).

## What shipped

**The detection design went through four revisions**, each one
invalidated by digging deeper or by a real spike, before landing on
something small. Revisions 1–3 (diffing `/sys/fs/cgroup/memory.events`
around each command, then a continuously-polled ledger, then tracking
"did smelt itself request this `SIGKILL`") all assumed an OOM kill could
plausibly take out *one process* — a leaf command, or worst case the
shell running it — while the rest of the pod carried on. A real spike
against the actual cluster (a genuine `cargo build` with a `build.rs`
memory bomb, run through the exact persistent-shell mechanism
`sandbox_agent` uses) disproved that: the whole container died every
time, confirmed via `pods.get()` — `state.terminated.reason: OOMKilled`.
Tuning `oom_score_adj` to `-1000` on the shell changed nothing. Directly
checking why: `/sys/fs/cgroup/memory.oom.group` is `1` on this cluster —
cgroup v2's "kill every process in the cgroup atomically" setting, not
"pick the single worst offender." Since the agent, every shell, and
every command in them all share one container's cgroup, there was never
a "just the leaf process died" case to attribute in the first place.

**What actually shipped instead:**
- **A per-pod memory/cpu limit, as a default the model can override.**
  `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` (`8Gi`/`1`) set the default;
  `create_pod` gains optional `memory_limit`/`cpu_limit` parameters that
  override it for just one pod. Bounded from above by a Kubernetes
  `LimitRange` in `smelt-park` (`max: 64Gi`/`16`) — the API server
  rejects an over-limit request automatically, no app-side quantity
  parsing or comparison needed.
- **Proactive, not reactive, crash detection.** `connect()`'s background
  reader task now reacts to its own WebSocket ending instead of just
  logging — the model finds out the moment a pod dies, not lazily on
  whatever tool call happens to touch it next (if any).
- **Confirmed via the Kubernetes API, with retry, before declaring a
  crash.** A single failed reconnect attempt no longer means "dead" — a
  transient portforward/API hiccup gets a few retries first;
  `pod_death_reason` (a pure decision function over an already-fetched
  `Pod`, exhaustively unit-tested, plus a thin real-fetch wrapper) is the
  authoritative signal, not the connection attempt itself.
- **Any confirmed crash — not just "gave up without ever confirming" —
  now fully terminates the pod**, not just its terminals: the k8s object
  is deleted and `sandbox_pods.terminated_at` is set, so `create_pod`
  never stays blocked behind a stale live-pod row regardless of which
  path caught the crash.
- **One pod-level notification per crash**, carrying whatever reason
  Kubernetes actually gave (`OOMKilled` included, passed through
  verbatim — no special-casing, no guessing at a cause the way the
  earlier revisions tried to).
- A deliberate `terminate_pod`/`teardown_conversation` is never
  misreported as a crash — `Arc::ptr_eq`-based identity checking on the
  connection registry, race-safe because both deliberate-teardown paths
  now deregister *before* they delete.

**Real bugs found and fixed along the way, beyond what the plan
anticipated:**
- `ensure_pod_connection`'s very first connection attempt to a freshly
  created pod (no agent injected yet — an expected failure, not a crash
  signal) was initially routed through the new retry-and-force-terminate
  logic, which force-terminated brand-new pods before they ever got a
  chance to have an agent injected. Caught by
  `test_terminal_lifecycle_end_to_end` failing with `NoPod`.
- The process-global `MANAGER` singleton holds a `kube::Client` whose
  internals are tied to whichever tokio runtime first constructed it — a
  second test racing to set it left a pre-existing test using a client
  whose background worker died with the *other* test's runtime,
  `Kube(Service(Closed))`. Same class of hazard `docs/testing.md` already
  documents for `PgPool`, just a second instance of it. Fix: don't add a
  second racer — fold new coverage into the one test that already safely
  owns `MANAGER`.
- The real-OOM-trigger integration test needed two rounds of debugging
  `AttachedProcess` lifecycle behavior: an exec whose stdout/stderr are
  never read can leave the remote command stalled rather than running,
  and — separately — dropping the `AttachedProcess` itself (not just its
  split-off stream handles) closes the exec session, which kills a
  directly-attached (non-`setsid`-detached) remote process right along
  with the disconnect. Both had already been hit and worked around
  earlier in the same session's exploratory spikes, but had to be
  rediscovered because that knowledge wasn't written down anywhere
  durable until this doc.

**Verification:** 24 real-cluster `sandbox::` tests (pure unit tests for
`decide_pod_death_reason`'s branching; the existing lifecycle test
extended with proactive-detection, deliberate-teardown, exhausted-retry,
and idle-terminal phases; two new standalone tests — a genuine,
lightweight OOM trigger, and the `LimitRange` rejection, which needed
`docker compose up k3s-bootstrap` — not just restarting `k3s` — to
actually deploy the manifest before it could pass), 64 `anthropic::`
tests, both build targets clean.

## Retrospective

**What worked:**
- Spiking against the real cluster, repeatedly, before committing to a
  design — three successive detection mechanisms (cgroup diffing, a
  continuous ledger, `oom_score_adj` tuning) got built and then discarded
  in conversation *before* any of them were implemented, because each
  spike's finding invalidated the assumption the previous design was
  built on. None of that churn happened in code.
- TDD discipline caught real bugs during implementation, not just
  validated already-correct code — the `ensure_pod_connection`
  regression, the `MANAGER` staleness bug, and both `AttachedProcess`
  lifecycle bugs were all found because a test failed unexpectedly.
- Reusing existing patterns instead of inventing new ones: the pure
  decision function was a direct copy of `check_pod_guard`/
  `resolve_pod_id`'s existing shape; the `connect()`/
  `reconnect_or_confirm_crash` `async fn` cycle got the same boxed-future
  fix `run_turn`/`execute()`'s own cycle already established; the
  pod-level notification reused `wake_conversation`'s existing delivery
  path outright.

**What caused friction, surprise, or rework:**
- The very first spike (a controlled, single-process memory bomb) gave a
  misleadingly narrow picture of the failure mode — it took a second,
  more realistic spike (a real `cargo build`) to find the actual dominant
  behavior (whole-container death), which invalidated three prior
  detection designs built on the first spike's incomplete picture. A
  controlled trigger is good for confirming a mechanism once you already
  know roughly what to expect; it's not a substitute for spiking against
  something shaped like real usage when you don't yet know what the
  failure actually looks like.
- Deploying the `LimitRange` required knowing that `docker compose up
  k3s` alone doesn't re-run the separate one-shot `k3s-bootstrap`
  service, and that the app's own service account correctly lacks
  permission to self-apply cluster-admin-level config — neither was
  discoverable except by trying and reading the resulting error.

**What to change:**
- Proposing a refinement to `development-process.md`'s existing "spike
  the riskiest assumption first" rule: for a failure-mode or
  resource-limit assumption specifically, the first spike should use a
  workload shaped like real usage (a real build, a real multi-process
  tool), not an artificially controlled trigger — the controlled version
  belongs *after*, to confirm a mechanism you already understand, not as
  the thing that establishes what the failure mode actually is.
- Proposing a short addition to `docs/testing.md` capturing the two
  `AttachedProcess` lifecycle gotchas (undrained output stalls the remote
  command; dropping the process object early kills a non-`setsid`-detached
  remote process on disconnect) — this is the second time in one project
  they've had to be rediscovered from scratch, and any future
  `pods.exec`-based spike or test will hit them again otherwise.

Both process changes above are proposed, not yet applied — following
the confirm-before-change rule, pending the user's go-ahead.
