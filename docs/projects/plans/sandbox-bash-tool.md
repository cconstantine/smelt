# Wire the sandbox to the conversation: a real `bash` tool

**Branch:** `sandbox-bash-tool`

## What

The first real tool: `bash`, dispatched through the existing
`anthropic::tools::execute` round trip exactly like `add`/`count` are
today, except its work happens inside that conversation's `Sandbox`
(`src/sandbox.rs`, shipped in the prior `k8s-sandbox` project) instead of
in-process. This is the "wiring" step the sandbox-lifecycle plan
explicitly deferred — the sandbox primitive already works; nothing today
calls it from a conversation.

Per the idea doc's own sequencing ("start with a small, real tool set...
grow it once the round-trip... and the sandbox proxy are solid"), this
plan is deliberately just `bash` — one tool, wired end to end, provable
from a real conversation. Explicitly **not** in this plan:

- File read/write tools — the idea doc's other named starting tool,
  deferred to its own follow-on rather than doubling this plan's surface.
- Running `bash` via `run_async` (background execution) — a natural
  pairing (a long build in the background) but its own integration with
  `run_async`'s wrapping/streaming machinery, not free to add here.
- Streaming `exec` output into `ChatEvent`/the browser — the idea doc's
  "Visibility" section already flags this as needing its own design; a
  `bash` call blocks the turn loop until it finishes, same as `count`
  already does today.
- A coding-oriented system prompt (`CreateMessageRequest.system`) — left
  as `None`, unchanged. Whether smelt's framing should shift from "chat
  agent that can also run shell commands" to "coding agent" is the open
  question `coding-session.md` itself raises about updating `state.md`'s
  framing; not decided here, just not blocked on either — the tool is
  available in *every* conversation regardless (see `How`), same as every
  other tool today.
- Real sandbox image (git, coreutils, a real toolchain) — `busybox:1.36`
  (chosen for the lifecycle plan's own narrower proving-the-plumbing
  purposes) is enough to prove `bash` end-to-end via `sh -c`; still an
  open question for whenever a tool actually needs more than a shell.
- The idle-timeout reaper for orphaned sandboxes — still deferred; this
  plan adds one more sandbox-creation path (a `bash` call) and one
  cleanup path (`delete_conversation`), not the general sweep.

## Which files

- `src/sandbox.rs`:
  - `pub fn init(client: kube::Client) -> &'static SandboxManager` /
    `pub fn get() -> &'static SandboxManager` — a process-global
    singleton via `OnceLock`, mirroring `db::init()`/`db::get()` exactly.
  - A per-conversation registry,
    `LazyLock<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Option<Sandbox>>>>>>`
    — same two-level-locking shape as `api::chat`'s `CONVERSATION_LOCKS`
    (a sync `Mutex` guarding the map's structure, an async `Mutex` per
    conversation guarding the actual `Sandbox` so a slow `exec` on one
    conversation doesn't block another's).
  - `pub async fn exec_for_conversation(conversation_id: i64, command: &[&str]) -> Result<ExecResult, SandboxError>` —
    looks up (or lazily `create`s, on first call) that conversation's
    `Sandbox`, then calls `exec` on it while holding its per-conversation
    lock, so two `bash` calls for the same conversation never run
    concurrently against the same pod.
  - `pub async fn delete_for_conversation(conversation_id: i64) -> Result<(), SandboxError>` —
    removes the registry entry and, if a `Sandbox` had actually been
    created, calls `SandboxManager::delete` on it. A no-op `Ok(())` if the
    conversation never used `bash`.
- `src/main.rs` — after the existing `rustls`/`dotenvy` setup and before
  `db::init()` (no ordering dependency between them, just keeping
  startup's infra-init calls together): build a `kube::Client` and call
  `sandbox::init(client)`.
- `src/anthropic/tools.rs`:
  - `tool_definitions()` gains a `bash` entry: `{"command": string}`,
    described as running inside a persistent, isolated sandbox scoped to
    the conversation.
  - `execute`'s match gains `"bash" => bash_tool(conversation_id, input).await` —
    alongside `list_tasks`/`cancel_task`, not through
    `execute_synchronous`, since it needs `conversation_id` and those
    don't.
  - `bash_tool` calls `sandbox::exec_for_conversation(conversation_id, &["sh", "-c", &command])`
    and formats the `ExecResult` into the single string `execute` returns
    (stdout, stderr if non-empty, and the exit code if non-zero — see
    `How`).
- `src/api/chat.rs` — `delete_conversation` also calls
  `sandbox::delete_for_conversation(id)` after the DB delete succeeds;
  failure is logged (`tracing::error!`) but doesn't fail the whole call —
  the DB delete is the user-visible contract, a leaked pod here is the
  same already-documented class of gap the idle-timeout reaper will
  eventually close, not a new one.
- Tests: `src/sandbox.rs` (registry reuse — see `How`), `src/anthropic/tools.rs`
  (`bash` dispatch), `src/api/chat.rs` (`delete_conversation` actually
  tears down a conversation's sandbox). All three need the same real
  cluster the existing sandbox tests already require.
- `docs/architecture.md`, `docs/projects/state.md` — `bash` moves from
  "the natural next project" to a listed feature; `add`/`count` remain
  documented as the throwaway stand-ins they still are.

## How

**Command shape.** `bash`'s `input_schema` takes one field, `command:
string`, run as `sh -c "<command>"` inside the sandbox — not an argv
array, matching the shape a model already expects from a shell tool (and
from Claude Code's own bash tool). Each call is a fresh `sh -c` process:
**shell state (cwd, env vars, background jobs) does not persist between
calls**, only the underlying container's filesystem does — `cd /work &&
some-command` in one call doesn't leave the next call starting in
`/work`. Worth stating plainly in the tool's own `description` so the
model doesn't need to discover it by trial and error.

**Output formatting.** `execute`'s dispatch returns `Result<String,
String>` (an `Ok` becomes a `ContentBlock::ToolResult`, an `Err` becomes
one flagged as an error — same as every other tool today), not a
structured type — so a successful `ExecResult { stdout, stderr, exit_code
}` collapses to one string, e.g.:

```
{stdout}
[stderr]
{stderr}
[exit code: {exit_code}]
```

with the bracketed sections included only when non-empty/non-zero, so a
clean, successful command's result is just its stdout with no noise
around it.

**Registry reuse, concretely.** `exec_for_conversation`'s first call for
a given `conversation_id` does the (potentially several-second) `create`
— pod scheduling, image pull if not cached, wait-for-`Running` — inline,
blocking that turn the same way `count`'s real sleep already blocks a
turn today. Every subsequent call for the same conversation reuses the
already-`Running` pod via the same async-`Mutex`-guarded registry entry,
so the cost is paid once per conversation's lifetime (or once per smelt
restart, since the registry itself is in-memory only — but the
*underlying pod* survives a restart regardless, per the lifecycle plan's
"Restart behavior," so a `create` after a restart is a cheap reuse, not
another cold pod build).

**Testing**, same posture as the existing sandbox tests (real cluster,
not mocked, unique ids per test):

- `exec_for_conversation` reuse: call it twice for the same conversation
  id — first call writes a file (`echo hi > /tmp/marker`), second call
  reads it back (`cat /tmp/marker`) and asserts the content survived —
  proof the *same pod* served both calls, stronger than just comparing
  pod names.
- `tools::execute(..., "bash", {"command": "echo hello"})` returns `Ok`
  containing `"hello"`; a nonzero-exit command's result string includes
  the exit code.
- `delete_conversation` on a conversation that has an active sandbox:
  assert the pod is actually gone afterward (mirrors the sandbox
  lifecycle plan's own `manager.delete` test).

## Open questions / tradeoffs

- **Should `bash` be everywhere or gated?** Every conversation gets every
  tool today (`tool_definitions()` is unconditional) — `bash` follows that
  same rule here rather than introducing the first per-conversation
  toggle. Worth a deliberate look once smelt's purpose/framing question
  (see `coding-session.md`) gets resolved, but not blocking this plan.
- **First-call latency.** A cold `bash` call could take several seconds
  (image pull + schedule) with nothing visible to the user during that
  window beyond however the UI already renders "the assistant is still
  working" — the idea doc's Visibility section already covers this gap in
  general; not solving it specifically for `bash` here.
- **Output size.** No truncation on `stdout`/`stderr` before they become
  part of the model's own context — a command that produces megabytes of
  output would blow well past a reasonable turn size. Not addressed; the
  `run_async` task registry has a similar known gap (no output pagination)
  already documented in `state.md`.
