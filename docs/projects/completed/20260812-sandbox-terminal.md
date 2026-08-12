# A persistent terminal, queried on demand

**Branch:** `sandbox-terminal` · **Idea:** `projects/ideas/coding-session.md` (kept — the terminal/exec piece is done, but file tools, UI visibility, git checkout, and the coding-oriented system prompt are still open) · **Plan:** `projects/plans/sandbox-terminal.md` (removed)

## What shipped

A genuinely persistent, interactive shell the model can create, run
commands in, interrupt, and tear down — reached through a purpose-built
**sandbox agent** injected into a Kubernetes pod, wired up as eleven real
tools. Delivered in two rounds: the original design (one pod, one
terminal, one command in flight, per conversation), then extended to N
pods per conversation, each with N terminals, before the branch closed.

- **Pod, terminal, and command are three separate, explicitly-guarded
  lifecycles** — `create_pod`/`terminate_pod`/`list_pods`,
  `create_terminal`/`terminate_terminal`/`list_terminals`, and
  `run_terminal_command`/`send_signal`/`terminal_command_status`/
  `read_terminal_output`/`list_commands`. A level can't be torn down while
  the level below it is still live (`terminate_pod` refuses while a
  terminal exists in it; `terminate_terminal` refuses while a command is
  still running in it).
- **Pull-based, not ambient**: the model asks for terminal output
  explicitly (`read_terminal_output`, bounded offset/limit) rather than
  having a scrollback force-fed into every request. A small completion
  notification (an ordinary persisted `user` message) is the one thing
  still pushed, when a background command finishes.
- **N pods, each with N terminals** — `src/db.rs` gained `sandbox_pods`/
  `sandbox_terminals` tables (soft-deleted via `terminated_at`, so a
  terminated terminal's command history still survives and is queryable);
  identity is DB-backed integer ids, not the tool call's own
  `tool_use_id` (which can contain uppercase and isn't durable across a
  restart). One agent **process** per pod, multiplexing every terminal
  that pod hosts over its single WebSocket connection — not one process
  per terminal — so terminals in the same pod share its filesystem and
  installed state, the way multiple shells in one real sandbox would.
- **`src/bin/sandbox_agent.rs`**: a small `axum`+`tokio` binary,
  `include_bytes!`'d into the server and landed in the pod via tar-over-
  exec (the same trick `kubectl cp` uses), launched via `setsid`. Holds a
  `HashMap<terminal_id, Shell>`; each shell is a real persistent `bash`
  (`set -m` + `trap ':' INT`) spawned with its own process group
  (`.process_group(0)`) so `terminate_terminal` can `killpg` one terminal
  without touching the agent or any sibling terminal in the same pod.
  `send_signal` delivers a real signal (default: `SIGINT`, like Ctrl-C)
  directly to a running command's process group via a syscall, discovered
  reactively through `/proc/<bash_pid>/task/<bash_pid>/children` —
  bypassing bash's stdin entirely, so the signaled command's shell state
  (cwd, exported variables) is provably untouched either way.
- **Crash detection is cleanup-only, not automatic recovery.** If a pod's
  agent goes unreachable, the next terminal-touching call for that pod
  marks every orphaned running command `'lost'` (notified once, same as a
  normal completion) and marks every one of that pod's terminals
  terminated — but never touches the pod itself or relaunches anything.
  Recovery is the model's job, through the same tools it already has.
- **12 sandbox.rs tests** (5 pod-lifecycle, unchanged from the prior
  round, plus one large end-to-end integration test against a real
  cluster and real Postgres covering: guards firing before anything
  exists to guard against, N distinct pods/terminals from repeated
  create calls, two terminals in one pod with independent shell state and
  genuine concurrency — a long command in one doesn't block a sibling —
  cross-pod filesystem isolation, `send_signal` resolving a command
  without disturbing its terminal, per-terminal guards, idempotent
  repeat-terminate, an unknown-id real error, and history surviving a
  terminated terminal), **20 db.rs tests**, and the existing **30
  tools.rs / 7 chat.rs** tests, all passing.

Explicitly not done (per the plan's own scope, and `coding-session.md`'s
still-open items): file read/write tools, `tty: true`/genuinely
interactive programs, a coding-oriented system prompt, an idle-timeout
reaper, `NetworkPolicy`, any cap on how many pods/terminals a conversation
can accumulate, streaming terminal output into the browser's live event
stream (`read_terminal_output` is pull-only, nothing pushes to
`ChatEvent`), and `git clone`/credential wiring.

## Retrospective

**What worked:**
- **Spiking against a real cluster caught two genuine, non-obvious bugs
  that no amount of design review or local (non-containerized) testing
  would have found** — see below. The plan's own discipline of writing
  "Spiked and verified" as a checklist, then actually running the
  integration test against the real cluster before calling anything done,
  is what surfaced both.
- **The plan conversation itself found real design bugs before any code
  was written** — the backgrounded-command-framing idea (`eval '...' &`)
  breaking `cd`/`export` persistence, and sending `SIGINT` to a job's
  process group taking the whole shell down with it, were both caught by
  small local spikes proposed during planning, not discovered in
  production. Cheap to fix at that stage; expensive after.
- **Layering pod → terminal → command as three separately-guarded
  lifecycles**, arrived at through several rounds of the user pushing back
  on an initially-simpler "one thing, created lazily" design, made the
  later N-pods/N-terminals extension a genuinely contained change — the
  guard rails and list tools were already shaped for it, so lifting the
  "one per conversation" limit was mostly about identity (DB-backed ids)
  and multiplexing (one WebSocket per pod), not a redesign of the tool
  surface itself.

**What caused friction, surprise, or rework:**
- **A `SIGINT` delivered via `killpg` silently did nothing, for a cause
  entirely unrelated to the process-group mechanics `trap ':' INT` was
  built to handle.** The agent runs as a descendant of the pod's PID 1
  (`sleep infinity`), which never installs a `SIGINT`/`SIGQUIT` handler —
  the kernel's PID-1 rule then forces those to `SIG_IGN`, which (unlike a
  caught signal) survives `exec()` all the way down into the agent, and
  POSIX forbids a non-interactive shell from overriding a signal already
  ignored "on entry." The fix (reset both to default disposition once, at
  agent startup, before spawning anything) was simple once found — but
  finding it took reading `/proc/<pid>/status`'s `SigIgn` bitmask by hand
  after the mechanism that was *supposed* to work (verified in an earlier,
  non-containerized spike) mysteriously didn't. **A container-specific
  quirk a host-level spike structurally cannot surface.**
- **The `pods/portforward` RBAC grant used the wrong verb**, discovered
  the same way: `create_terminal` 403'd every time despite the Role
  clearly including `pods/portforward`. kube-rs's WebSocket-based
  portforward is an HTTP `GET` under the hood, which Kubernetes authorizes
  as `get`, not `create` — nothing in the plan's own research surfaced
  this distinction, since SPDY-based portforward (the older mechanism,
  and what most examples online show) *does* use `POST`/`create`. Found
  by a raw HTTP probe against the API server returning the exact
  Kubernetes error text, not guessed at.
- **An already-applied migration couldn't be edited for the N-pods/N-
  terminals schema change**, requiring a second migration file rather than
  revising the first — expected per `docs/migrations.md`'s own rule, but
  worth remembering: once a migration has touched the real dev database
  (not just `#[sqlx::test]`'s throwaway ones), it's locked.

**What to change (proposals):**
- **When a design depends on signal delivery, container-namespace
  behavior, or anything else PID-1/container-runtime-specific, spike it
  inside the actual target container early — a host-level spike proving
  the same mechanism isn't sufficient evidence.** Both real bugs this
  round surfaced only once the *actual* integration test ran against the
  *actual* pod; a spike step that deliberately ran the send_signal
  mechanism inside a throwaway pod (not just locally) before writing the
  full agent would likely have caught the PID-1 issue at the same low
  cost the existing spikes caught the framing/SIGINT-propagation bugs.
- **For any new k8s subresource RBAC grant (`exec`, `portforward`,
  `attach`, ...), verify the actual HTTP verb via a raw probe or
  `SelfSubjectAccessReview` before writing the Role, rather than inferring
  it from the resource name or older examples** — `create` is a reasonable
  but wrong guess for a WebSocket-based subresource.
