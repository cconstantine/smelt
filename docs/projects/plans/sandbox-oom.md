# Sandbox memory limit + reacting to a pod dying

**Branch:** `sandbox-oom`

**Status: on hold**, resuming now with a much narrower detection design
than any earlier revision — see "Detection design history" below for why.
The `512Mi`/`500m` limit this plan was originally written against no
longer exists — `CPU_LIMIT`/`MEMORY_LIMIT` and `build_pod_spec`'s
`resources: Some(ResourceRequirements { ... })` block were removed
outright (not just raised) as an immediate, separate fix: pods are
currently unbounded, limited only by whatever the node itself has to
give. Reintroducing the limit itself (`SANDBOX_CPU_LIMIT`/
`SANDBOX_MEMORY_LIMIT`) is unchanged across every revision of this plan —
see "Which files."

## What

Two related problems: the sandbox pod currently has *no* CPU/memory limit
at all (a temporary stopgap, not the fix — see "Status"), and when the
pod dies under whatever limit eventually comes back, the model isn't told
until (if ever) it happens to touch that pod again on its own.

**Reintroduce the limit as a default, not a fixed value — let the model
override it per pod.** New `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` env
vars (same default-if-unset-or-empty pattern `anthropic_model()` already
uses), defaulting to `8Gi`/`1` (a full core), set the *default* a pod gets
when nothing else is specified. `create_pod` gains two optional
parameters, `memory_limit`/`cpu_limit` (plain Kubernetes `Quantity`
strings, e.g. `"4Gi"`, `"2"`), letting the model ask for more headroom up
front for a pod it already knows will do something memory-hungry, instead
of being stuck with one deployment-wide number regardless of what the
conversation is actually about. **Sizing the default now carries more
weight than it did in earlier revisions of this plan** — see the next
section for why.

**Proactively detect the pod dying and tell the model.** Not "detect
which *command* OOMed" — see below for why that goal was dropped. Just:
the moment the sandbox pod becomes unreachable for a reason smelt didn't
itself cause, say so, instead of leaving the model to find out (or not)
the next time it happens to try that terminal — and if Kubernetes itself
reports why (an `OOMKilled` container status, say), pass that along too,
since confirming the pod is actually dead already means fetching it.

## Detection design history (why this is the fourth pass, and much smaller than the third)

Revisions 1–3 (diffing `memory.events`, a continuous ledger, then
"did smelt itself request this `SIGKILL`") all assumed the same thing:
that an OOM kill under a real, enforced pod memory limit could plausibly
kill *one process* — a leaf command, or (worse case) the shell running
it — while the rest of the pod, agent included, carried on. All three
designs existed to attribute a death to a specific command or terminal
under that assumption.

**A real spike against this cluster disproved the assumption.** Ran an
actual `cargo build` (a `build.rs` that allocates memory, first in one
700MB shot, then paced in small 20MB chunks) inside a pod with a real,
enforced `512Mi` limit, through the exact persistent-shell mechanism
`sandbox_agent` uses (`eval`, foreground job, `echo MARKER:$?`). Every
run: the shell died with no marker, and — checked directly via
`pods.get()` afterward — **the whole container died with it**,
`state.terminated.reason: OOMKilled`, `exit_code: 137`. Setting the
shell's own `oom_score_adj` to `-1000` (verified to actually take effect
— confirmed by reading it back, `CAP_SYS_RESOURCE` granted via the pod's
`securityContext`) changed nothing: shell and container still died
together, identically. Checked why directly: `/sys/fs/cgroup/memory.oom.group`
is `1` in this cluster. That's the cgroup v2 setting that makes the
kernel kill *every process sharing the cgroup, atomically*, once the
limit is breached — not "pick the single worst offender." Since the
agent, every shell it's spawned, and every command running in those
shells all share that one container's cgroup, hitting the limit for
*any* reason takes all of them down together, every time. There is no
"just the leaf process died" case to attribute from a real OOM under
this deployment's actual runtime behavior — `oom_score_adj` cannot change
this, because it only influences which single process gets picked when
the kernel *is* picking one, which isn't what's happening here.

**So there's nothing left to detect at the command/shell granularity for
this cause.** The only thing worth building is what's actually
observable and actually true: the pod became unreachable. No attempt is
made to *infer* a cause from ambiguous signals (an exit code, a bare
`SIGKILL`) the way revisions 1–3 tried to — that's what didn't survive
the spike. But Kubernetes' own container status is a different thing
entirely: an *authoritative* fact, not an inference, and it's already
being fetched as part of confirming the pod is actually dead (see
"Detection design" below) — so when it's there, pass it straight through
to the model instead of throwing it away in favor of generic wording.

**One consequence worth being explicit about:** because `restart_policy:
Never` (matches `build_pod_spec` today), the container does not come
back on its own once this happens — the pod's phase goes to `Failed` and
stays there; whoever's managing the conversation has to notice and call
`create_pod` again, and anything that only existed in that container's
own (unmounted-volume, ephemeral) filesystem is gone with it. Hitting
`SANDBOX_MEMORY_LIMIT` doesn't fail one command — it ends the whole
sandbox session. That's the real stakes behind sizing the default
generously, and behind proactively telling the model right away rather
than leaving it to find out.

## Per-pod limit overrides

`create_pod`'s two new parameters are plain pass-through strings — no
app-side parsing or validation of the `Quantity` format itself. An
invalid value (`"lots"` instead of `"4Gi"`) is rejected by the
Kubernetes API when the pod is actually created, surfacing back through
the existing `SandboxError::Kube` path as an ordinary tool error, the
same way any other malformed request to Kubernetes already surfaces
today — nothing new to build for that case.

**Ceiling: a Kubernetes `LimitRange`, not app-side clamping.** Nothing in
the override mechanism above stops the model from requesting an
unreasonably large limit — the whole reason a limit exists at all is to
bound how much one sandbox can take from the node, and a per-pod override
that isn't itself bounded quietly reopens that. A `LimitRange` in the
`smelt-park` namespace, with a `max` for container memory/cpu, has the
API server reject any pod creation exceeding it automatically —
model-provided override or not — with zero app-side comparison logic.
Chosen over app-side clamping (a `SANDBOX_MAX_MEMORY_LIMIT`/
`SANDBOX_MAX_CPU_LIMIT` env var pair, which would need real parsing/
comparison of Kubernetes quantity strings — `Ki`/`Mi`/`Gi`, millicore `m`
suffixes — logic-bearing code this project's process would want tested,
not a trivial addition) specifically because the groundwork already sits
there unused: the `park` Role in `k8s/smelt-park-rbac.yaml` already has
`list`/`get` on `limitranges`, and that file already establishes the
idempotent-apply-via-`k3s-bootstrap` pattern a new object slots straight
into — no new plumbing, just one more YAML document in a file that
already exists for exactly this kind of namespace-level configuration.
Only sets `max` (not `default`/`defaultRequest`) — `build_pod_spec`
always sets `resources.limits` explicitly itself (the env-var default or
the model's override), so a `LimitRange` default would never actually
apply; no reason to set one and imply otherwise.

## Detection design

**Make the existing crash-handling proactive instead of reactive.**
`handle_crash_cleanup` (`sandbox.rs`) already does everything needed once
it runs: marks every running command in the pod `'lost'`, marks every
live terminal terminated, publishes UI events, and pushes a notification
via `wake_conversation`. The existing per-command wording stays as-is —
"Terminal command `{id}`'s outcome is unknown — the terminal became
unreachable while it was running." — since there's no per-command reason
to attach (Kubernetes reports on the *container*, not on an individual
`terminal_commands` row). What's missing is *when* this runs: today, only
`reconnect_if_needed` calls it, and that only happens when some other
terminal-touching call happens to run against that pod later. If the
model doesn't try that pod again, it never finds out at all.

**Trigger it from the connection's own close, not from the next lazy
check.** `connect()`'s background reader task (`sandbox.rs`) already
notices the exact moment the WebSocket ends, for any reason — today it
just calls `deregister(pod_id)` and logs. Change it to run
`handle_crash_cleanup` right there, immediately — *unless* this closure
is the expected result of a deliberate `terminate_pod`/
`teardown_conversation` already in progress, which must not be reported
as a crash.

**Distinguishing "deliberate" from "crash" without new state.** The
existing registry (`TERMINAL_CONNECTIONS`, one entry per pod) already
gives us this for free if `deregister` is made identity-aware: the reader
task, on its own connection ending, only treats it as a crash if the
registry's *current* entry for that `pod_id` is still the exact same
`Arc<TerminalConnection>` it holds (checked via `Arc::ptr_eq`) — meaning
nobody has already removed or replaced it. `terminate_pod`
already calls `deregister(pod_id)` as part of its own flow; reordering it
to run *before* `pods.delete()` (today it's after — a one-line swap)
means the registry entry is already gone by the time the k8s deletion
actually causes the WebSocket to drop, so the reader task's identity
check correctly finds "already handled" and skips crash-reporting.
`teardown_conversation` already deregisters before deleting — no change
needed there.

**Confirm via the Kubernetes API before declaring a crash — don't trust a
single failed connection attempt.** Making detection proactive removes
the cushion the old lazy/reactive design had: a transient WebSocket
failure (a brief K8s API hiccup, the agent momentarily slow to accept a
new connection right after the old one dropped) is no longer given time
to resolve itself before something notices — it would get reported as a
permanent crash on the first bad attempt. The pod's own status via
`pods.get()` is the authoritative signal for whether that's actually true:
gone (`get_opt` → `None`), `phase: Failed`, or a container status already
showing `terminated` (checking *both* `state` and `last_state` — see the
earlier-found `restart_policy: Never` quirk, where Kubernetes reports
`OOMKilled` via `state`, not `last_state`, when there's no restart to
make them differ) is confirmation — declare the crash immediately,
no point retrying a connection that can't succeed. Anything else
(`Running`, `Pending`, or the status check itself failing) is
inconclusive — retry the connection a few times with a short backoff
before giving up. This also fixes an existing, related bug:
`reconnect_if_needed`'s current "pod exists but isn't `Running`" branch
treats `Pending` and `Failed` identically, silently returning an error
without ever running cleanup for a pod that has, in fact, permanently
died — `Failed` should be folded into the same "confirmed dead" check as
"pod is gone entirely," not treated as "might still come up."

**Add one pod-level notification, once, alongside the existing per-command
ones — carrying whatever reason Kubernetes actually gave, when it gave
one.** Today a crash only ever produces per-*command* messages (silent
for an idle terminal with nothing running). Given the new understanding
that a crash always takes the *whole* pod down, `handle_crash_cleanup`
gains a single top-level message, persisted once per crash (gated on the
same "found at least one live terminal to clean up" condition that
already makes the function a harmless no-op on a redundant second call,
so no new dedup state is needed), surfaced the same way the existing
per-command messages already are. The confirmation check above is already
looking at the container's `terminated.reason` to decide "is this dead" —
threading that same string through to the message is free, not new work:
- Reason is anything Kubernetes reported (`Error`,
  `ContainerStatusUnknown`, ...): included the same way, generically —
  "Sandbox pod `{pod_id}` stopped unexpectedly (`{reason}`); every
  terminal running in it is no longer available."
- No reason available at all (the pod object is gone entirely with
  nothing left to inspect, or retries were exhausted without Kubernetes
  ever confirming anything) — falls back to today's plain wording,
  "Sandbox pod `{pod_id}` stopped unexpectedly; every terminal running in
  it is no longer available." — not a guess dressed up as one.

## Which files

- **`src/sandbox.rs`**:
  - `default_memory_limit()`/`default_cpu_limit()` functions reading
    `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` (default `"8Gi"`/`"1"`),
    same pattern as `anthropic_model()` — named `default_*` rather than
    the fixed `MEMORY_LIMIT`/`CPU_LIMIT` constants earlier revisions
    proposed, since they're no longer the only value a pod can get.
  - `build_pod_spec` takes explicit `memory: &str, cpu: &str` parameters
    (already resolved — default-or-override — by its caller) instead of
    reading the env-backed functions itself, and regains a
    `resources: Some(ResourceRequirements { limits: Some(limits),
    ..Default::default() })` block built from them.
  - `create_pod` gains `memory_limit: Option<String>, cpu_limit:
    Option<String>` parameters; resolves each against its `default_*`
    function when `None`, and threads the resolved pair down through
    `SandboxManager::create` (which gains the same two parameters,
    passed to `build_pod_spec` only on the actual-creation branch — its
    existing reuse-an-already-Running-pod branch has nothing to apply
    them to, since resources are immutable on an existing pod).
  - `deregister` gains an identity-checked variant (`Arc::ptr_eq` against
    the registry's current entry) alongside the existing unconditional
    one `terminate_pod`/`teardown_conversation` use for their own
    deliberate teardown.
  - `connect()`'s reader task: on WebSocket end, call the identity-checked
    deregister; if it confirms this connection was still the live one
    (nobody tore it down first), attempt the confirm-and-retry sequence
    below instead of declaring a crash immediately.
  - `terminate_pod`: swap the order of its `pods.delete()`/`deregister()`
    calls (deregister first) so a deliberate teardown always wins the
    race against the reader task noticing the connection drop.
  - New `async fn pod_death_reason(pods: &Api<Pod>, name: &str) -> Option<Option<String>>`
    (admittedly awkward double-`Option` — outer: "did we get authoritative
    confirmation at all," inner: "did Kubernetes give a specific reason
    string for it") — `None` for `Running`/`Pending`/an unreachable API
    call (nothing proven either way, keep retrying); `Some(reason)` once
    confirmed dead, where `reason` is `terminated.reason` off whichever of
    `state`/`last_state` actually has it, falling back to `pod.status.reason`
    (set directly by Kubernetes for e.g. `Evicted` pods) if the container
    status itself doesn't have one, or `None` if the pod is gone entirely
    with nothing left to inspect. Split like `check_pod_guard`/
    `resolve_pod_id` already are in this file: a thin async wrapper that
    just does the `pods.get_opt` fetch, handing the result to a pure
    `fn decide_pod_death_reason(pod: Option<Pod>) -> Option<Option<String>>`
    that contains all the actual branching — so every branch is
    unit-testable by constructing a `Pod`/`PodStatus` value directly, no
    cluster needed (see "Testing").
  - New `async fn reconnect_or_confirm_crash(pool, pod_id) -> Result<Arc<TerminalConnection>, TerminalError>`
    — the shared retry loop both paths below use: try `connect()`; on
    failure, check `pod_death_reason`; if it confirms death (`Some(_)`),
    run `handle_crash_cleanup(pool, pod_id, reason)` and return
    `NoTerminal` immediately; if inconclusive (`None`), back off briefly
    and retry, up to a small bounded attempt count. If attempts are
    exhausted without ever confirming death *or* succeeding, still run
    `handle_crash_cleanup(pool, pod_id, None)` (the terminal is unusable
    either way) rather than leaving the caller in limbo — and, since
    Kubernetes itself isn't going to clean up a pod it still thinks is
    healthy, also make a best-effort attempt to terminate the pod
    ourselves: a fresh `pods.delete()` plus `db::terminate_sandbox_pod`,
    reusing whatever `terminate_pod` already does for that part (see
    below) rather than duplicating it. Best-effort and non-blocking —
    log a failure, don't retry it, don't let it change what's returned to
    the caller (`NoTerminal` either way). Without this, a pod stuck in
    this state would otherwise sit unreachable-but-technically-`Running`
    indefinitely, consuming node resources, *and* leave the conversation's
    `sandbox_pods` row looking live — blocking `create_pod` from ever
    making a fresh one without the model first discovering it has to call
    `terminate_pod` manually.
  - `terminate_pod`'s own delete-and-mark-terminated tail (past its
    live-terminals guard) factors out into a small shared
    `async fn force_terminate_pod(pool, pod_id) -> Result<(), SandboxError>`
    — best-effort `pods.delete()` (tolerating "already gone"),
    `db::terminate_sandbox_pod`, and the existing `SandboxPodUpdate{
    terminated: true }` event publish — used by both `terminate_pod`
    (after its guard passes) and `reconnect_or_confirm_crash`'s exhausted-
    retries fallback above (which has no guard to check: `handle_crash_cleanup`
    just ran and already cleared every live terminal for this pod).
  - `reconnect_if_needed`'s `connect()`-failure branch, and its "pod
    exists but isn't `Running`" branch, both route through this instead
    of their current one-shot logic — folding in the `Failed`-should-be-
    treated-as-dead fix described above.
  - `handle_crash_cleanup` gains a `reason: Option<String>` parameter and
    the one pod-level notification described above (branching on `reason`
    exactly as described), persisted directly via `db::create_message`
    before the existing `wake_conversation` spawn. No other change — the
    per-command path (marking commands `'lost'`, the existing wording) is
    untouched.
- **`src/anthropic/tools.rs`**: `create_pod`'s `ToolDefinition.input_schema`
  gains two optional string properties, `memory_limit`/`cpu_limit`
  (neither in `required`), with description text explaining they override
  the deployment default for just this one pod. Its dispatch arm gains
  `input` (today it's the one pod/terminal tool called without it) and
  `create_pod_tool` reads both via `input.get(...).and_then(Value::as_str)`,
  same pattern every other optional-parameter tool here already uses,
  passing them through to `sandbox::create_pod` as `Option<String>`.
- **`docs/setup.md`**: new `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` rows
  in the env var table, same shape as the `ANTHROPIC_*` ones, documented
  as defaults rather than fixed values.
- **`k8s/smelt-park-rbac.yaml`**: one new `LimitRange` document (this file
  is already multiple `---`-separated objects applied together, not
  RBAC-only despite the name — it's the namespace's whole bootstrap
  manifest):
  ```yaml
  apiVersion: v1
  kind: LimitRange
  metadata:
    name: sandbox-pod-max
    namespace: smelt-park
  spec:
    limits:
      - type: Container
        max:
          memory: 64Gi
          cpu: "16"
  ```
  `64Gi`/`16` — a generous multiple of the `8Gi`/`1` default, not measured
  against anything — same flagged-guess status as the default itself. Per
  this file's own header comment, `homelab`'s copy of
  this scope was created by hand and predates this file, and has to be
  updated to match by hand too — this new object is no exception, and
  that step is easy to forget since nothing automates it.

Nothing in `src/bin/sandbox_agent.rs` or `src/db.rs` needs to change — no
new schema, no new per-command field, no agent-side changes at all. Every
earlier revision's plumbing for attributing a cause to a specific command
(`oom_killed`/`unexpected_kill` columns, ledgers, reapers, `oom_score_adj`
tuning) is dropped, not deferred — it solved a problem that turned out
not to exist under this cluster's actual OOM behavior.

## How

- The identity check (`Arc::ptr_eq`) is what makes "was this closure
  expected" correct under the real race that matters: a deliberate
  `terminate_pod` deletes the k8s pod, which *causes* the WebSocket to
  drop shortly after — the reader task's own close-handling and
  `terminate_pod`'s explicit cleanup are racing to be "first," and
  whichever one wins must suppress the other's crash-reporting. Ordering
  `deregister` before `pods.delete()` in `terminate_pod` guarantees it
  wins that race every time, rather than depending on timing.
- The pod-level message's one-shot guarantee comes from
  `handle_crash_cleanup`'s existing behavior, not new tracking: it queries
  live terminals for the pod and does nothing if there are none — true on
  any call after the first, whether that's a genuine second crash
  notification attempt or the pre-existing reactive path (`reconnect_if_needed`
  → `pods.get_opt` returns `None`) still firing afterward for an unrelated
  reason.
- No attempt is made anywhere in this design to distinguish OOM from any
  other reason the pod might die (node eviction, the node itself going
  away, ...) — deliberately, since the whole premise of the earlier
  revisions (that OOM specifically was distinguishable and worth calling
  out) didn't survive the spike.
- `pod_death_reason` is intentionally conservative: an API call that
  itself fails (the K8s API server briefly unreachable) is treated the
  same as "inconclusive, retry" (`None`) — never as confirmation either
  way. A smelt restart racing its own reconnect against a genuinely dead
  pod isn't a special case here; it just means the retry loop's first
  couple of attempts fail before `pod_death_reason` catches up and
  returns `Some(_)`, same as any other real crash.
- The reason string is passed through verbatim, `OOMKilled` included,
  rather than translated/prettified case by case — Kubernetes' own
  vocabulary here (`OOMKilled`, `Error`, `ContainerStatusUnknown`, ...) is
  small and already reasonably legible; a per-reason translation table is
  more maintenance than the model needs for what's meant to be
  informational context, not a UI string.

## Testing

**Every failure mode this plan describes gets a real, permanent test —
these are not spikes.** The spikes run earlier in this project (the
real `cargo build` OOM trigger, the `oom_score_adj`/`memory.oom.group`
checks) were throwaway by design: written, run, and deleted, existing
only to validate an assumption before committing to a design. Nothing
here is that. Every behavior below is asserted by a test that ships in
the same PR as the code and stays in the tree afterward, same as every
other test in `sandbox.rs`'s existing module — this plan isn't done when
the feature works once against a real cluster by hand, it's done when
the test suite proves it keeps working.

**Pure decision logic — unit-tested, no cluster needed:**
- `decide_pod_death_reason`, exhaustively: pod gone (`None` in →
  `Some(None)` out); `Failed` with a container `terminated.reason` in
  `state`; the same but in `last_state` instead (the `restart_policy:
  Never` quirk — `state` populated, `last_state` empty); `Failed` with no
  container status at all, falling back to `pod.status.reason`; `Failed`
  with neither available; `Running`/`Pending` → `None` (inconclusive);
  `OOMKilled` specifically flows through unmodified, same as any other
  reason string.
- `check_pod_guard`/`resolve_pod_id` already have this coverage from
  before this plan — no change needed, just the same pattern extended to
  the new function.

**Real-cluster integration tests — kept in `sandbox.rs`'s existing
`#[tokio::test]` module, not `#[ignore]`d, same as the tests already
there:**
- A pod deleted out from under a live connection (`pods.delete()` — the
  same cheap, fast way `test_dropping_without_delete_still_cleans_up_via_drain_task`
  already simulates pod death, not a real OOM trigger, since what's under
  test here is smelt's *reaction*, not the kernel's OOM killer, which the
  earlier spike already validated once) is proactively detected via the
  connection's own close, without needing another tool call to notice.
- `handle_crash_cleanup` marks the affected commands `'lost'`, marks
  terminals terminated, and sends exactly one pod-level notification.
- A `terminate_pod` call is never misreported as a crash — deleting a
  pod deliberately produces no "stopped unexpectedly" notification.
- A connection failure against a pod that's still genuinely `Running` is
  retried, not immediately declared a crash (simulate by having
  `connect()` fail once — e.g. a bad/racing portforward — while the pod
  itself is left alone).
- A `Pending` pod isn't treated as dead; a `Failed` one is, immediately,
  without waiting out the retry budget.
- Retries exhausted without ever confirming death still runs
  `handle_crash_cleanup` *and* force-terminates the pod (`force_terminate_pod`
  actually deletes the k8s object and marks `sandbox_pods.terminated_at`,
  verified by a subsequent `create_pod` succeeding immediately after,
  without the model needing its own explicit `terminate_pod` call first).
- An idle terminal with nothing running still gets covered by the
  pod-level message when its pod crashes.
- The pod-level message's reason-branching against `pods.delete()`-simulated
  deaths: present and generic (a constructed non-`OOMKilled` reason, or
  whatever `Failed`-without-a-clean-reason actually produces against the
  real API), and absent (pod gone entirely, message falls back to the
  plain wording).
- `create_pod` with a `memory_limit`/`cpu_limit` override actually
  produces a pod with that resource limit set (checked via `pods.get()`
  after creation), and omitting either falls back to
  `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT`.
- `create_pod` with an override exceeding the `LimitRange` max fails,
  surfacing the Kubernetes-originated rejection as an ordinary tool
  error — this test only passes once the `LimitRange` from "Which files"
  is actually applied to the test cluster (`k3s-bootstrap`), which
  doubles as a check that the manifest change itself is live, not just
  written.

**One test using a genuine, real OOM trigger — not simulated by
`pods.delete()`.** The pure unit tests exhaustively cover
`decide_pod_death_reason`'s branching against *constructed* `Pod`
values, and the integration tests above cover smelt's reaction to a pod
that's merely gone — neither actually proves Kubernetes really reports
`OOMKilled` the way the earlier spike found, or that a real one flows
correctly through the whole live path end to end. Reusing the
lightweight technique from that very first spike (a bash `printf -v`
memory bomb — no fork, no `rust:*` image or `cargo build` needed, unlike
the later cascade spike) against a pod created with a small
`memory_limit` override (e.g. `"64Mi"`, using this same plan's own
override feature to trigger fast rather than needing to fill the real
`8Gi` default), asserting: the container genuinely gets OOM-killed,
`pod_death_reason` extracts `"OOMKilled"` from the *real* API response
(not a constructed one), and the resulting pod-level notification text
actually contains it. This is the one place the reason-branching logic
gets checked against what Kubernetes actually does, not just what the
unit tests assume it does.

## Open questions

- **A single shell dying while the agent and its siblings survive** was a
  real gap identified earlier (`run_reader` hits EOF and silently drops
  the in-flight command forever — no marker, no `'lost'` marking, no
  notification) via reasoning about `eval`-run builtins ballooning the
  *shell's own* memory, before the `memory.oom.group` finding. Given that
  finding, this specific *cause* (OOM) can no longer produce this
  shape — a real OOM always takes the whole pod, agent included. The gap
  itself isn't fully closed by this plan, though: something other than
  OOM (a bash bug, a stray signal) could still kill one shell in
  isolation, and nothing here catches that. Not building it now, since
  it's a different, smaller problem than what motivated this plan —
  flagging it as a separate, still-open item rather than silently
  dropping it.
- **`SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` defaults.** `8Gi`/`1` — the
  stakes of under-sizing this are now understood to be higher (losing the
  whole session, not one command), which argues for erring generous
  rather than tight. Still a guess pending real usage.
- **`LimitRange` `max` values (`64Gi`/`16`) are a guess**, same as the
  `8Gi`/`1` default — cheap to retune later (a `kubectl apply`, not a
  rebuild), but not measured against real usage yet.
- **Remembering to apply the updated manifest to `homelab`, not just the
  compose test cluster** — this file's existing convention (see its own
  header comment), not something this plan changes, but worth restating
  since it's an easy step to drop.
- **An idle terminal with nothing running, in a pod that crashes**, is
  covered by the new pod-level message but gets no individual mention of
  its own — accepted as sufficient; a per-terminal message on top would
  be redundant with "every terminal running in it is no longer
  available."
- **Retry count/backoff for `reconnect_or_confirm_crash` are guesses** —
  proposing 3 attempts, ~1s apart, but not measured against how long a
  real transient blip (portforward re-establishment, a brief API server
  hiccup) actually takes to resolve on this cluster. Cheap to retune
  later, same as the other not-yet-sized constants in this plan.
- **Exhausting retries without ever confirming death via the API** (pod
  stuck reporting `Running` while genuinely unreachable — a hung agent
  inside a technically-alive container, say) results in
  `handle_crash_cleanup` running (same cause-agnostic wording as a
  confirmed crash — "became unreachable" already doesn't overclaim a
  specific cause, no separate "uncertain" message variant needed) *and*
  now a best-effort attempt to actually terminate the pod (see "Which
  files"), rather than leaving a resource-consuming zombie the model
  can't `create_pod` past.
- **Should the *confirmed*-dead branch (`Some(reason)`) also force-terminate
  the pod, not just the exhausted-retries branch?** Not decided here —
  the user's ask was specifically about the exhausted/unconfirmed case.
  A confirmed-dead pod (`Failed`, or `get_opt` → `None`) isn't consuming
  live resources the way an unconfirmed zombie might be, but it *does*
  leave the same `create_pod`-blocking stale `sandbox_pods` row behind,
  and a `Failed` pod object has nothing else that will ever clean it out
  of the namespace either — same shape of problem, arguably, just lower
  urgency. Worth revisiting before implementation rather than leaving an
  inconsistency between the two branches by accident.
