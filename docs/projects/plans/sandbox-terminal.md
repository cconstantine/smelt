# A persistent terminal, queried on demand

**Branch:** `sandbox-terminal`

## What

A genuinely persistent, interactive shell per conversation, reached through
a purpose-built **sandbox agent** running inside a k8s pod. Two design
decisions carry the whole plan and are covered in depth in `How`: the model
gets at terminal output by **asking for it**, not by having it force-fed
into every request (see "Why pull-based, not ambient"); and **pod,
terminal, and command are three separate, explicitly-managed lifecycles**,
not one implicit chain — each with its own create/terminate/list tools, and
each guarded so a level can't be torn down while the level below it is
still live.

**Eleven tools, grouped by level:**

| Level | Tools |
|---|---|
| Pod | `create_pod()`, `terminate_pod()`, `list_pods()` |
| Terminal | `create_terminal()`, `terminate_terminal()`, `list_terminals()` |
| Command | `run_terminal_command(command)`, `send_signal(command_id, signal)`, `terminal_command_status(command_id)`, `read_terminal_output(command_id, stream, offset, limit)`, `list_commands()` |

**Creation is layered, not lazy.** `create_terminal()` requires a pod to
already exist — that requirement is the entire point of separating the two
— and is where the agent actually gets injected and launched (moved out of
pod-creation, where an earlier draft of this plan had it). `run_terminal_
command()` requires a terminal to already exist. Neither auto-creates what
it depends on; both return a clean `is_error: true` telling the model what
to create first. `create_pod()` and `create_terminal()` are otherwise
idempotent — calling either when the thing already exists is a no-op
success, not an error, mirroring `SandboxManager::create`'s existing
get-or-create pattern.

**Teardown is guarded in the same direction it's built:** `terminate_pod()`
refuses if a terminal still exists (`terminate_terminal()` first);
`terminate_terminal()` refuses if a command is still `running`
(`send_signal`/wait it out first). `terminate_pod()` is idempotent too —
no pod, no-op success.

**Crash detection no longer recovers automatically.** An earlier version of
this plan had the server silently delete and recreate the pod the moment
an agent was found unreachable. Once pod lifecycle became something the
model explicitly owns via `create_pod`/`terminate_pod`, doing that behind
its back stopped making sense — the model would have no way to know its
pod had been swapped out from under it. Now: detecting the agent is gone
only does cleanup (any `terminal_commands` row still `running` is marked
`'lost'` and notified, the connection registry entry is cleared) — it does
not touch the pod. Recovery is the model's job, through the same tools it
already has: `terminate_pod()` (now unblocked, since crash-cleanup already
cleared the terminal), `create_pod()`, `create_terminal()`.

**`send_signal(command_id, signal)` replaces the "just kill it" idea a
few drafts of this plan went through.** Sending `SIGINT` to a running
command is meant to behave like hitting Ctrl-C in a real terminal: the
*command* may die (or handle it gracefully, or ignore it entirely — same
as a real terminal), but the shell itself, and everything about its state
— cwd, exported variables — is untouched, because the signal never goes
through bash at all. See `How`'s "Signaling a running command" for the
mechanism; the short version is that the agent tracks a PID it learned
when the command started and signals it directly via a syscall, bypassing
bash's stdin entirely.

Still explicitly **not** in this plan: file read/write tools, `tty: true`/
genuinely interactive programs, a coding-oriented system prompt, the
idle-timeout reaper, `NetworkPolicy`, multiple pods or multiple terminals
per conversation (both considered, both deliberately deferred — one
in-flight command per terminal is still the only concurrency this plan
supports).

## Which files

- **`k8s/smelt-park-rbac.yaml`** — gains `pods/portforward` (`create`).
  Same already-flagged follow-up as the base RBAC file: apply to
  `homelab` by hand too.
- **A new binary target**, `src/bin/sandbox_agent.rs` (`axum`+`tokio`+
  `serde_json`, all already dependencies):
  - On startup, spawns one persistent inner shell (`bash`, piped stdio)
    with **`set -m` (job control) and `trap ':' INT`** — both required,
    for two different reasons verified by spike (see `How`). Holds it for
    the agent's whole lifetime, replacing it only on the next
    `create_terminal` after a `terminate_terminal`.
  - Per `{"id", "command"}` message: writes the **same two-line payload
    originally verified** — `eval '<escaped>'` then a marker echo,
    foreground, unchanged. An attempt earlier in this round to background
    this line (to capture a PID via `$!`) broke `cd`/`export` persistence
    and was reverted — see `How`'s "Command framing and the wedge risk."
  - PID/process-group discovery for `send_signal` happens **out of band,
    via `/proc/<bash_pid>/task/<bash_pid>/children`, read reactively only
    when `send_signal` is actually called** — not polled eagerly right
    after a command starts, and not reported by bash itself. No change
    to the framing above, and no timing requirement on the common,
    never-signaled path. Since only one command is ever in flight, the
    agent only needs to track *which* `command_id` is currently running
    (already required for the in-flight check), not a PID map. See `How`.
  - New WS message handled: `{"id": "<command_id>", "signal": "INT" |
    "TERM" | "KILL"}` — if `id` matches the currently in-flight command,
    reads its process group from `/proc` and signals it directly via a
    syscall (e.g. `nix::sys::signal::killpg`); otherwise a no-op/error.
    The normal `exit` event still fires whenever bash's marker line
    arrives, signaled or not — completion detection is entirely
    unaffected by any of this.
  - Launched via **`setsid`**, not just `nohup ... &` — this plan had left
    that choice open before; it's settled now, because `setsid` makes the
    agent both a session and process-group leader, and bash (an ordinary
    child, not separately `setpgid`'d away) inherits that same group.
    That's what lets `terminate_terminal` kill agent-plus-idle-bash with
    one process-group signal while never touching the per-command groups
    job control creates. The agent writes its own PID to a fixed path
    (e.g. `/tmp/sandbox_agent.pid`) at startup so the server can target it
    later without needing to ask the agent.
  - A normal dynamically-linked build, same toolchain and target as the
    main server — unaffected by anything in this round.
  - Base image change, unaffected and carried over: `busybox:1.36` →
    `debian:trixie-slim`, matching the dev image's Debian release, so
    `bash` is actually present.
- **`migrations/YYYYMMDDHHMMSS_create_terminal_commands_and_events.sql`**
  — unchanged this round. PID and process-group bookkeeping for signaling
  and termination lives entirely in the agent's own memory, never
  persisted — same reasoning that already removed the in-memory exit-code
  slot from the server's own registry: nothing here is on a path that
  needs to survive a restart or be queried by anything other than the
  live agent process itself.
- **`src/db.rs`** (extended) — everything from the prior round
  (`create_terminal_command`, `terminal_command_is_running`,
  `append_terminal_event`, `mark_terminal_command_finished`,
  `mark_terminal_command_lost`, `terminal_command_status`,
  `read_terminal_output`, `unnotified_finished_terminal_commands`,
  `mark_terminal_command_notified`), plus:
  - `list_terminal_commands(pool, conversation_id, limit) ->
    Vec<TerminalCommand>` — backs `list_commands`, most-recent-first,
    bounded the same way `read_terminal_output` already is.
- **`src/sandbox.rs`** (restructured around the three levels):
  - **Pod**: `create_pod`/`terminate_pod`/`list_pods` are thin wrappers
    over `SandboxManager`'s existing create/delete, now genuinely bare —
    no injection, no launch, nothing agent-related happens here anymore.
    `terminate_pod` checks the connection registry first and refuses (no
    k8s call at all) if a terminal is registered.
  - **Terminal**: `create_terminal`/`terminate_terminal`/`list_terminals`
    is where injection and launch now live — tar-over-exec to land the
    agent binary (embedded via `include_bytes!`), the `setsid`-based
    detached launch described above, `Api<Pod>::portforward` plus a
    WebSocket handshake held for the conversation's lifetime.
    `create_terminal` requires `list_pods` to show a pod first; connect
    logic still branches on create-vs-reuse the way the prior round
    described, to tell "never launched" apart from "agent's gone" —
    that distinction is unchanged, only what happens on the "agent's
    gone" branch is different now (cleanup only, detailed below, not
    pod deletion). `terminate_terminal` refuses if
    `terminal_command_is_running` is true; otherwise execs `kill -- -
    $(cat /tmp/sandbox_agent.pid)` into the pod and clears the registry
    entry.
  - **Crash detection, now cleanup-only.** Wherever a terminal-touching
    call finds the agent unreachable on a pod that already had one: mark
    any `running` `terminal_commands` row `'lost'`, let the existing
    notification path pick it up, and clear the registry entry — full
    stop. No pod deletion, no relaunch. This directly supersedes "Agent
    crash recovery: recreate the pod, not the process" from the prior
    round; see `How` for why.
  - The per-conversation registry is unchanged in shape — still just the
    WebSocket connection handle, nothing else.
  - The background task draining WS messages into `terminal_events` /
    `terminal_commands` is unchanged — the one internal marker line is
    filtered on the *agent* side, same as always, and never reaches the
    wire; PID/process-group discovery for `send_signal` doesn't touch
    bash's stdout at all, so there's nothing new for this task to handle.
- `src/anthropic/tools.rs` — the eleven tools from `What`, each a thin
  wrapper over the `sandbox.rs`/`db.rs` functions above plus the guard
  checks, all under the existing per-conversation lock so two tool calls
  racing within one parallel-tool-use assistant turn can't both pass a
  guard. `read_terminal_output` and `list_commands` both clamp their
  bound server-side (exact caps still not decided — see Open Questions).
- `src/api/chat.rs`, `src/main.rs` — unchanged from the prior round: the
  completion-notification check inside `run_turn`'s loop, `delete_
  conversation` tearing down whatever pod/terminal exist, `sandbox::init`
  at startup.

## Protocol

- Client → agent, unchanged: `{"id": "<uuid>", "command": "<shell
  command>"}`
- Client → agent, new: `{"id": "<uuid>", "signal": "INT" | "TERM" |
  "KILL"}`
- Agent → client, unchanged shape: one message per output line
  (`{"id", "stream", "seq", "data"}`), one `exit` message
  (`{"id", "event": "exit", "code"}`) — `seq` is still the agent-assigned
  per-command counter from the prior round, unaffected by anything here.

The command-to-bash framing has exactly one internal line (the completion
marker), exactly as originally verified — filtered out before anything
reaches the WebSocket, same as always. PID/process-group discovery for
`send_signal` doesn't touch this protocol or bash's stdout at all — it's
a separate, out-of-band `/proc` read the agent does on its own (see
`How`), so there's nothing new here to filter.

## How

**Why pull-based, not ambient.** Unchanged from the prior round: an
earlier draft rendered a bounded scrollback into every single API request
regardless of relevance, which is paid at full price every call and is
the wrong granularity for what the model actually needs. Real output only
ever appears as an ordinary `tool_result`, requested explicitly; a small
completion notification is the one thing still pushed, deliberately kept
tiny for the same reason.

**Ordering: why the agent assigns `seq`, not the server.** Unchanged: the
agent is the only place that actually observes stdout/stderr's true
interleaving, so it's the only place that can assert order correctly —
relying on the server's insertion order matching agent-read order is an
invisible invariant, not a property of the stored data.

**Three explicit, guarded lifecycles instead of one implicit one.** Pod
and terminal used to be one thing, created lazily the first time a command
needed either. Splitting them serves the same goal the pull-based tool
design does: give the model explicit, checkable state instead of ambient
assumptions. `list_pods`/`list_terminals`/`list_commands` mean the model
never has to guess or remember — it can always ask what currently exists.
The guards (`terminate_terminal` needs no running command;
`terminate_pod` needs no terminal) exist so a teardown can never
accidentally take something out from under work that's still in flight,
without needing per-tool special-case error handling to catch it —
"command still running" and "terminal still exists" are the only two
invariants to hold, checked the same way every time.

**Agent crash recovery is no longer automatic — cleanup only, model
recovers explicitly.** The prior round's answer to a crashed agent was
"delete and recreate the pod automatically" — a good answer when pod
lifecycle was entirely the server's business, but it stopped being a good
answer the moment `create_pod`/`terminate_pod` made pod lifecycle
something the model explicitly owns. Silently replacing the model's pod
out from under it defeats the purpose of exposing those tools at all. So
now: detecting the agent is unreachable does exactly what it needs to and
no more — mark the orphaned command `'lost'`, notify, clear the registry
— and stops there. The pod itself is left exactly as it was (however that
turns out — still there with a dead agent inside it, from the model's
perspective indistinguishable from "no terminal"). Recovery is an
ordinary `terminate_pod` → `create_pod` → `create_terminal` sequence,
using tools the model already has, not a hidden path underneath them.

**Command framing and the wedge risk.** Unchanged from the very first
spike in this plan, and confirmed to stay that way: the risk is a
syntactically-incomplete command (unclosed quote, unterminated heredoc,
trailing `\`) left running as raw text leaving bash waiting for more
input, wedging every command sent afterward. The mitigation is unchanged
too — wrap the command as `eval '<escaped>'`, so the *outer* line bash
parses is always complete regardless of the command's own content,
followed by a marker echo. **An attempt earlier in this round to
background this line (`eval '<escaped>' & echo "PID:$!"`, to learn the
command's PID for `send_signal`) was reverted after a spike found it
broke shell-state persistence:** `cmd &` forks a new process to run
`cmd` even when `cmd` is a builtin like `cd` or `export`, and a builtin's
effect on shell state (cwd, environment) only ever applies to the process
that actually executes it — a `cd` running inside a forked-for-
backgrounding child changes that child's cwd, then the child exits and
the change is gone, having never touched the persistent shell at all.
Confirmed directly: with the backgrounded framing, `cd /tmp` followed by
a separate `pwd` call reported the *original* directory, not `/tmp`. **This
is ordinary `fork()` semantics, not a bash surprise** — any state change
made inside a forked child was always going to die with that child; the
miss was proposing the backgrounded framing in the first place without
tracing through that it would fork for a builtin, not anything bash did
unexpectedly once tried. The fix was to revert the framing entirely, not
patch around it — see the next two sections for how PID discovery and
signaling work instead, without the command line itself changing at all.

**Discovering a running command's process group without changing the
framing.** With the command line back to a plain foreground `eval`, the
agent needs another way to learn what to signal later. Verified by spike:
`/proc/<bash_pid>/task/<bash_pid>/children` lists a process's direct
children, kernel-maintained, entirely outside bash's own cooperation.
**This is read reactively, only when `send_signal` is actually called —
not polled eagerly right after the command starts.** That matters: an
eager poll timed to the write would be exactly the race it looks like,
guessing at how long a fork takes on every single command whether or not
anything ever signals it. Reactive lookup has no such requirement on the
common path — by the time a model calls `send_signal`, the command has
necessarily been running long enough to be worth interrupting, so
whatever it forked (if anything) has had plenty of time to show up in
`/proc`. The only real race left is a `send_signal` arriving essentially
back-to-back with `run_terminal_command`, before bash has necessarily
finished forking — handled with a short bounded retry (a few polls over
a few tens of milliseconds), not assumed away; if nothing ever appears,
that's treated as "nothing to signal" (the command already finished, or
never forked at all — a bare builtin), not an error. With `set -m` (job
control) enabled, every job bash runs —
including a plain foreground one, and including a pipeline — gets its own
process group distinct from bash's own, and every stage of a pipeline
shares that one group; confirmed directly by starting a foreground
`sleep 5 | cat | sleep 6` and finding all three stages under one process
group in `/proc`. That means the agent doesn't need to track a PID at
all, let alone one per command — since only one command is ever in
flight, `send_signal` just re-reads `/proc` at the moment it's called and
signals whatever process group is there; a stale or mismatched
`command_id` is rejected against the one the agent already tracks for its
own in-flight check, not against anything PID-related.

**Signaling a running command without disturbing the terminal.**
`send_signal` delivers the requested signal as a direct `killpg()`
syscall from the agent's own process, targeting the process group
discovered above — bash is never asked to do anything, never even needs
to be responsive at that instant (it's sitting in a plain blocking
`wait`, exactly as it always has, unaware anything happened). That's why
shell state survives: nothing about bash's own execution changes, only
the signaled process's. **Unlike the `cd &` finding above, this one is
genuinely non-obvious, not a design miss in hindsight:** sending `SIGINT`
to a foreground job's process group killed *bash itself* too, even though
bash was never a member of that group — a non-interactive bash, by
default, re-raises `SIGINT` against itself once
it notices (via `wait`) that a foreground job died from `SIGINT`,
standard POSIX shell behavior for this exact scenario (it's what makes
Ctrl-C during a `sleep` inside a plain script also stop the script).
`trap '' INT` (silently ignore) looked like the fix but isn't one:
`SIG_IGN` is preserved across `exec()`, so the *job* would inherit the
ignore too and become unkillable by `SIGINT`. `trap ':' INT` (a real, if
no-op, handler) is what actually works — POSIX resets *caught* signals to
default on `exec()`, so the forked job gets ordinary `SIGINT` behavior,
while bash no longer treats the job's `SIGINT` death as a reason to end
its own life, since it now has an explicit trap installed. Confirmed
directly: with `trap ':' INT` in place, `killpg` on a running `sleep`'s
group with `SIGINT` kills the `sleep` (exit code 130, exactly the shell
convention), and the persistent shell — cwd, exported variables,
everything — is completely intact immediately afterward. `SIGTERM`/
`SIGKILL` never had this problem; it's specific to `SIGINT`'s
POSIX-mandated propagation rule.

**Terminating a terminal without touching the pod.** `terminate_terminal`
execs a single `kill -- -<pgid>` into the pod, using the PID the agent
wrote to `/tmp/sandbox_agent.pid` at launch — since the agent was launched
via `setsid`, that PID is also the process group ID shared by the agent
and its (guaranteed-idle, by the guard) inner bash, so one signal cleanly
takes out both. Nothing about the pod itself is touched; a later
`create_terminal` against the same pod just injects and launches fresh.

**Detached launch, concretely.** `setsid` is what keeps the agent alive
after its launching exec session closes — an exec'd process not
explicitly detached is liable to be torn down when the client disconnects
in most container runtimes, which would defeat the entire point of a
persistent agent. (This plan previously left `nohup ... &` and `setsid`
open as alternatives; `setsid` is the one actually used now, for the
process-group properties described above, not just detachment.)

**Build ordering.** Unchanged: the agent must be built first and
`include_bytes!`'d into the server, same toolchain and target as the
server itself — no cross-compilation, since the sandbox image matches the
dev image's Debian release. Still not decided whether that's a documented
two-command sequence or a `build.rs` step.

**Spiked and verified before writing any real implementation:**

1. ✅ A dynamically-linked build of the agent ran inside a
   `debian:trixie-slim` pod with no glibc mismatch.
2. ✅ Tar-over-exec injection landed a working, correctly-permissioned
   executable.
3. ✅ A detached, `setsid`-launched process survives its launching exec
   session ending — confirmed both directly in the original round (via
   `nohup ... &`) and again as part of item 9 below, using `setsid`
   specifically this time.
4. ✅ `Api<Pod>::portforward` succeeded against a real cluster with a full
   WebSocket handshake and protocol round-trip.
5. ✅ The agent's own axum-based WebSocket server behaved correctly
   inside the sandbox pod.
6. ✅ **The original two-line framing (`eval '<escaped>'` + marker),
   unchanged, against a live persistent bash with `set -m` and
   `trap ':' INT` added:** wedge-safe against unterminated heredocs,
   unclosed quotes, and trailing backslashes (all fail cleanly, shell
   stays responsive); `cd`/`export` persist correctly across separate
   calls. A backgrounded variant of this framing was also tried and
   **rejected** — see `How`'s "Command framing and the wedge risk" for
   why it broke `cd`/`export` persistence (ordinary `fork()` semantics
   the design proposal should have anticipated, not a bash surprise);
   not carried forward.
7. ✅ `set -m` does not leak bash's own job-control notification text
   into captured stderr, in the cases tested (a plain foreground command,
   and a command backgrounded by the model's own shell syntax that
   completes before the next call) — not exhaustively tested against
   every job-control interaction (a stopped job, several concurrent
   user-backgrounded jobs), which the model isn't expected to rely on
   anyway.
8. ✅ `killpg` on a job-control-assigned process group reaches every
   stage of a multi-command pipeline (confirmed against a real 3-stage
   pipeline) and leaves bash itself untouched — *conditional on*
   `trap ':' INT` being installed; without it, `SIGINT` specifically
   (not `SIGTERM`/`SIGKILL`) takes bash down too. See `How`.
9. ✅ `terminate_terminal`'s `kill -- -<pgid>` against a `setsid`-launched
   process and its ordinary (non-`setpgid`'d) child cleanly takes out
   both. One methodology note worth keeping: a naive `/proc/<pid>`
   existence check taken immediately after signaling can show a false
   "still alive" during the brief window before the parent finishes
   exiting — `wait()`-based reaping is what actually confirmed this, not
   the existence check.

## Verification

Concrete, observable checks from the model's actual perspective, run
against a real conversation and the real Anthropic API, not mocked:

1. **Pod lifecycle is idempotent and honest.** `create_pod` twice in a
   row: second call is a no-op success, not an error, not a second pod.
   `terminate_pod` with no pod: no-op success. `list_pods` reflects
   reality before and after each.
2. **Terminal creation requires a pod.** `create_terminal` with no pod
   existing returns a clean `is_error: true` naming the missing
   precondition, not a lazily-created pod.
3. **Command execution requires a terminal**, same shape of check as
   above, against `run_terminal_command`.
4. **Guards actually block, not just document intent.** With a command
   running, `terminate_terminal` returns `is_error: true` and the command
   is unaffected. With a terminal existing, `terminate_pod` returns
   `is_error: true` and the terminal is unaffected.
5. **`send_signal` interrupts a command without resetting the
   terminal.** `cd` into a directory, start a long-running command,
   `send_signal` it with `INT`, confirm the command actually stops (or
   handles it — a well-behaved target process's graceful-shutdown
   response counts), then confirm a fresh `pwd`-equivalent still reports
   the directory from before the signal — proving shell state survived.
6. **A killed command still resolves cleanly.** After `send_signal`,
   `terminal_command_status` eventually reports `finished` with a
   signal-consistent exit code (not left stuck at `running`), and the
   usual completion notification fires exactly once.
7. **`list_commands` reflects reality, bounded.** Run several commands;
   confirm the list reflects them (id, status, exit info) most-recent-
   first, and confirm it's actually bounded rather than growing without
   limit as more commands accumulate.
8. **State persists across separate turns.** `cd` in one message; in a
   separate, later turn, confirm the model can report the directory via
   a fresh command without `cd`-ing again.
9. **Restart survives — shell state and recorded history**, unchanged
   check from before: prior shell state and prior commands' recorded
   status/output both remain correct after a smelt restart.
10. **A malformed command doesn't wedge the session** — re-run against
    the *new* framing specifically, not assumed to still hold just
    because the old framing passed it.
11. **Crash recovery is now the model's job, and it works end to end.**
    Kill the agent process directly. Confirm the next terminal-touching
    call detects it, marks any running command `'lost'` with exactly one
    notification, and that `terminate_pod` → `create_pod` →
    `create_terminal` afterward produces a working terminal again — and
    confirm the pod is genuinely gone and replaced, not just assumed to
    be, since nothing does that automatically anymore.
12. **Message history growth still reflects only what was requested** —
    unchanged check from before, re-run to confirm the new tools didn't
    reintroduce ambient bulk anywhere.

Worth turning at least items 4, 5, 7, 10, and 11 into a scripted (not
necessarily CI-automated, given the real-API dependency) smoke check.

## Open questions / tradeoffs

- **Dev-image/sandbox-image Debian coupling, accepted deliberately.**
  Unchanged from before — worth a cross-referencing comment at both the
  `Dockerfile`'s base image line and `build_pod_spec`'s image constant.
- **Agent dependency weight** — `axum` vs. a lighter WebSocket-only
  crate. Not decided, unaffected by this round.
- **`pods/portforward` on `homelab`** — same manual-apply follow-up as
  before.
- **The `eval` framing's safety still depends on the escaping being
  correct**, full stop — unchanged risk, unchanged framing, worth
  including in the same dedicated adversarial-input test this already
  called for.
- **Mid-restart message loss, not solved by this plan.** Unchanged: a
  line or exit event arriving in the exact window while smelt is
  disconnected from the agent could still be lost.
- **No retention policy for `terminal_commands`/`terminal_events`.**
  Unchanged — both are unbounded, permanent logs; only reads against them
  are bounded.
- **`read_terminal_output` and `list_commands`'s server-side bound
  aren't chosen yet** — needed regardless of what the model requests, so
  neither call can return an unbounded amount of content.
- **`send_signal`'s bounded retry window (for the back-to-back-with-
  `run_terminal_command` race) isn't sized yet either** — a few polls
  over a few tens of milliseconds is the working assumption, not
  measured. The spike didn't specifically stress-test a near-zero-delay
  `send_signal` call; worth doing before trusting the number.
- **Job-control stderr pollution wasn't found, but wasn't exhaustively
  tested either.** Spike 7 found no bash-generated job-control text
  leaking into captured stderr for a plain foreground command or a
  quickly-finishing model-backgrounded one. More exotic interactions (a
  stopped job, several concurrent model-backgrounded jobs) weren't tried
  — not expected to matter, since the model has no reason to reach for
  shell-level job control itself, but worth remembering if something
  surprising shows up later. If it ever does, the fix needs a real
  design, not pattern-matching bash's own notification text — that would
  reintroduce exactly the fragile text-parsing this whole agent design
  was built to avoid.
- **Single pod, single terminal, single command in flight — still the
  deliberate v1 scope**, not limitations discovered late. All three were
  considered and explicitly deferred; the guard rails and list tools in
  this plan are shaped to make lifting any one of them later a real but
  contained change, not a redesign.
