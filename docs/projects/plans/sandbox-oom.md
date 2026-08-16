# Sandbox memory limit + OOM detection/handling

**Branch:** `file-tools` (deliberately, not a fresh branch — folded in per
explicit instruction, alongside the file-tools and AUTH_TOKEN work already
in progress there; branches to be sorted out at commit time)

**Status: on hold.** The `512Mi`/`500m` limit this plan was originally
written against no longer exists — `CPU_LIMIT`/`MEMORY_LIMIT` and
`build_pod_spec`'s `resources: Some(ResourceRequirements { ... })` block
were removed outright (not just raised) as an immediate, separate fix:
pods are currently unbounded, limited only by whatever the node itself
has to give. That change was deliberately the *simple* fix, not this
plan — shipping a real, meaningfully-sized limit again without the
detection/attribution below would just recreate the original "way too
small, and unexplained when it bites" complaint in a different shape once
node contention or a single runaway process becomes the limiting factor
instead. This plan is what reintroduces a limit for real: sized
properly, configurable, and paired with knowing *why* something died
when it hits it. Resume by reading "Spiked and verified" below (still
accurate — the cluster's cgroup v2 behavior hasn't changed) and adjusting
"Which files" for the fact that `CPU_LIMIT`/`MEMORY_LIMIT` must be
re-added from scratch, not re-enabled.

## What

Two related problems, one plan: the sandbox pod currently has *no*
CPU/memory limit at all (see "Status" above — a temporary stopgap, not
the fix), and when a process gets OOM-killed under whatever limit
eventually comes back, nothing detects or surfaces that — it just looks
like an unexplained `exit code 137` (or, if the killed process is the
agent itself, an unexplained "terminal became unreachable").

**Reintroduce the limit, this time configurable.** `CPU_LIMIT`/
`MEMORY_LIMIT` were plain hardcoded constants before removal — today's
whole "way too small, and required editing and rebuilding the server to
fix" complaint shouldn't be possible to repeat. New
`SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` env vars (same
default-if-unset-or-empty pattern `anthropic_model()` already uses),
defaulting to `2Gi`/`1` (a full core) — both comfortably higher than the
original `512Mi`/`500m`, sized for a real `cargo build`-class workload
rather than the original "prove the pod boots" placeholder.

**Spiked first, against the real cluster** (this project's process
requires validating a container-runtime-specific assumption inside the
actual target container, not just reasoning about it) — see "Spiked and
verified" below. The two questions that mattered: what actually happens
when a process in the pod gets OOM-killed, and what's available inside
the container to detect it. Findings:

- **A leaf process being OOM-killed (e.g. a `cargo build` subprocess) is
  completely invisible at the Kubernetes pod/container status level.**
  Triggered a real OOM kill (a genuine anonymous-memory hog, confirmed by
  `exit_code 137`) against a pod with a real enforced `512Mi` limit —
  the pod's `phase` stayed `"Running"`, the container's `state`/
  `last_state`/`restart_count` showed nothing had happened at all. This
  rules out "check `pods.get()`'s container status" as the detection
  mechanism for the common case (the sandbox's own long-lived process,
  `sandbox_agent`, survives; only a short-lived child inside it died).
- **cgroup v2 is confirmed in use**, with `/sys/fs/cgroup/memory.events`
  (a per-cgroup, kernel-maintained counter file, including an `oom_kill`
  line) readable from inside the container. This is the real, portable
  detection mechanism: `sandbox_agent` — already long-lived, already
  present for exactly this pod's cgroup — can read this file directly.
- **`dmesg` is not accessible** (`Operation not permitted`) — rules out
  kernel-log-based detection entirely, confirming `memory.events` is the
  only viable in-container signal.
- A **whole-container OOM kill** (the agent's own process, or the pod's
  `sleep infinity` entrypoint, selected as the OOM victim instead of a
  leaf process) *would* show up in Kubernetes' container status
  (`last_state.terminated.reason == "OOMKilled"`) — not exercised live in
  the spike (the trigger reliably killed only the leaf process, not the
  agent), but this is standard, documented Kubernetes behavior, worth
  checking for cheaply in the one place that already handles "the agent
  went unreachable" (`handle_crash_cleanup`) in case it's ever the agent
  itself that goes down this way.

**Detection design:** `sandbox_agent` snapshots
`/sys/fs/cgroup/memory.events`'s `oom_kill` counter immediately before
starting a command and again right when it reports that command's exit
(the same moment it already emits the `MARKER_PREFIX` line) — if the
counter increased during the command's lifetime, the exit event carries
`oom_killed: true`. Precise to "something in this pod's cgroup got
OOM-killed while this command was running," not "this exact process was
the victim" — see Open Questions on the multi-terminal edge this
implies.

**Handling:** an OOM-attributed exit gets a distinct, explicit
notification — "Terminal command `{id}` was killed: it ran out of memory
(pod limit: `{limit}`)." — instead of the generic "finished: exit code
137," reusing `terminal-exit-notify`'s `wake_conversation`/backlog-drain
mechanism to actively push it, the same as any other completion. The flag
is also persisted (`terminal_commands.oom_killed`) and exposed through
`terminal_command_status`, so it's still discoverable even if the model
missed the initial notification. The whole-container case
(`handle_crash_cleanup`) gets an analogous text change — "...the terminal
became unreachable — its pod ran out of memory" instead of the current
generic wording — when the container status confirms `OOMKilled`.

## Spiked and verified

Ran directly against the real k3s cluster (temporary `#[tokio::test]` in
`sandbox.rs`, written, run, and deleted — not left in the tree): created a
real pod with a real `512Mi` limit enforced, confirmed cgroup v2 and
`memory.events`/`memory.max`/`memory.current` readable, triggered a real
OOM kill (a bash array filled via `printf -v`, genuine unreclaimable
anonymous memory — a naive growing string or a disk-backed `dd` write
both failed to actually trigger one: string concatenation was too slow to
outpace the timeout, and page-cache-backed writes are reclaimable so the
kernel never needed to invoke the OOM killer), confirmed `exit_code 137`,
and confirmed the pod/container status showed nothing while `memory.max`
correctly reflected the real limit.

One loose end: re-reading `/sys/fs/cgroup/memory.events` via fresh `exec`
calls immediately after the trigger came back empty (stdout and exit code
0) across several retries, including for a trivial `echo` — looks like an
artifact of opening many separate `pods.exec` sessions in a tight loop
right after a memory-pressure event, not evidence the file itself becomes
unreadable (the *first* read, before the trigger, worked normally, and
cgroup pseudo-files are a stable kernel interface, not something that
degrades). The real implementation reads this file from *inside*
`sandbox_agent`'s own already-long-lived process, never via a fresh
`exec`, so this specific oddity shouldn't apply — but Phase 2 should
confirm that concretely (read it for real, from the agent, around a real
triggered OOM) before leaning on it, rather than assuming the spike's
finding transfers cleanly.

## Which files

- **`src/sandbox.rs`**:
  - `CPU_LIMIT`/`MEMORY_LIMIT` (removed entirely — see "Status") come
    back as functions reading `SANDBOX_CPU_LIMIT`/`SANDBOX_MEMORY_LIMIT`
    (default `"1"`/`"2Gi"`), same pattern as `anthropic_model()` — not as
    plain constants again, so the original complaint (fixing this needs a
    rebuild) can't recur.
  - `build_pod_spec` regains a `resources: Some(ResourceRequirements {
    limits: Some(limits), ..Default::default() })` block (also removed
    entirely, along with the `BTreeMap`/`ResourceRequirements`/`Quantity`
    imports it needed) built from those two functions' values.
  - `handle_agent_message` parses the new `oom_killed` field off an
    `"exit"` event alongside the existing `id`/`code`, threads it into
    `db::mark_terminal_command_finished`.
  - `handle_crash_cleanup`: before marking commands `'lost'`, does one
    best-effort `pods.get()` for the pod and checks
    `container_statuses[0].last_state.terminated.reason ==
    Some("OOMKilled")`; if so, marks the affected running-turned-lost
    command(s) with the same `oom_killed` flag via
    `db::mark_terminal_command_lost`.
- **`src/bin/sandbox_agent.rs`**:
  - New pure function `parse_oom_kill_count(contents: &str) -> Option<u64>`
    — extracts the `oom_kill` line's value out of `memory.events`' text
    format. Unit-tested directly, no cluster needed.
  - New `async fn read_oom_kill_count() -> Option<u64>` — reads
    `/sys/fs/cgroup/memory.events` and applies the parser; `None` if the
    file's missing/unreadable (e.g. cgroup v1, or a non-Kubernetes dev
    environment) rather than erroring the whole command.
  - `start_command`: snapshot the count before writing the command to the
    shell's stdin.
  - The marker-handling path in `handle_socket`/`run_reader` (wherever the
    `MARKER_PREFIX` line is turned into `ShellEvent::Marker`/
    `ServerMessage::Exit`): snapshot the count again, compare, and add
    `oom_killed: bool` to `ServerMessage::Exit`.
- **`src/db.rs`**:
  - New migration adding `oom_killed BOOLEAN NOT NULL DEFAULT false` to
    `terminal_commands`.
  - `mark_terminal_command_finished`/`mark_terminal_command_lost` gain an
    `oom_killed: bool` parameter; `TerminalCommand`/`TerminalCommandStatus`
    gain the field.
- **`src/anthropic/tools.rs`**: `terminal_command_status_tool`'s JSON
  output includes `oom_killed`.
- **`src/api/chat.rs`**: `drain_unnotified_terminal_commands`'s
  notification-text construction branches on `oom_killed` for both the
  `finished` and `lost` cases, producing the distinct wording described
  above instead of the generic one.
- **`docs/setup.md`**: new `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` rows
  in the env var table, same shape as the `ANTHROPIC_*` ones.

## How

- `memory.events`' format is `key value\n` pairs (`low`, `high`, `max`,
  `oom`, `oom_kill`, `oom_group_kill`) — `parse_oom_kill_count` finds the
  `oom_kill` line and parses its value; malformed/missing content is
  `None`, not a panic.
- The before/after snapshot is taken once per command (not polled on a
  timer) — cheap (one file read), and precise to "did this counter change
  while this specific command was in flight," which is exactly the
  window that matters.
- No change to `run_terminal_command`'s own contract or `terminal_command_status`'s
  existing fields beyond the new addition — `oom_killed` defaults `false`
  and is purely additive.

## Open questions

- **Multi-terminal misattribution.** `memory.events` is per-pod-cgroup,
  shared by every terminal in that pod. If two terminals each have a
  command running concurrently and one process anywhere gets OOM-killed,
  *both* commands' exit windows would see the counter change, and
  whichever is still running when the other's exit is reported could get
  incorrectly tagged. Pinpointing the actual victim would need PID-level
  attribution (reading `memory.events` scoped to a specific process
  tree, or diffing `/proc` before/after) — real added complexity not
  clearly justified yet. Proposing to accept this as a known,
  reasonable-tradeoff limitation for this round: still strictly better
  than today's "no attribution at all," and multiple terminals each
  independently running something memory-hungry enough to trigger this
  is a narrow case.
- **`SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` defaults.** Proposing
  `2Gi`/`1` — a real step up from `512Mi`/`500m`, sized for a `cargo
  build`-class workload, but still a guess pending real usage. Easy to
  raise further later since it's now an env var, not a rebuild.
