# SME-78 plan: stop sandbox pods automatically

Revision of 2026-10-31. It replaces the plan cut at `e177c86` (2026-10-09), which the user approved on 2026-10-27, and folds in the 2026-10-27 "Additional scope". Ticket: [SME-78](https://linear.app/smelt-agent/issue/SME-78). Its Trade-offs to flag and Questions for the user are in the ticket, not here.

**Branch:** `sme-78-stop-idle-pods`, worktree `~/smelt-worktrees/smelt-sme-78`, cut from `origin/main` at `e3869e4`.

## What changed since the approved plan

* **SME-79 shipped as idle notices, not time limits** (PR #148, the user's 2026-10-15 rule "smelt must never stop a command on a timer"). There's no `time_limit_secs`, so the old `POD_BUSY` "command with a limit" clause and the `time_limit_seconds` edit are gone. Instead a model whose command goes untouched gets a notice every 5 minutes (`SANDBOX_IDLE_NOTICE_MINUTES`). Two consequences, each a question in the ticket:
  * An idle *pod* stop ends whatever is still running in the pod. That's approved answer 2, but SME-79's notice and the system prompt now promise "smelt won't stop it"; the plan rewords both (question 1).
  * Each notice is a message, which bumps `conversations.updated_at`, the old plan's activity signal: a forgotten dev server in a conversation the user hasn't stopped would draw a notice and a turn every 5 minutes and never be idle. The plan drops `updated_at` and counts turns by what started them (question 2).
* **The decisions that stand,** from the 2026-10-27 approval: 60 minutes by default, `SANDBOX_IDLE_STOP_MINUTES`, `0` = off; a server with no limit doesn't keep its pod alive on its own (question 1 checks it against SME-79); an open tab isn't activity; the stop notice doesn't move the conversation in the sidebar.
* **New scope:** a finished sub-agent's pod stops at once, at `finish` (below).
* **File map:** remapped onto `src/db/`, `docs/setup/`, `docs/api/`, `docs/architecture/` (see Files).
* **Merged since, checked for design changes:**
  * SME-253 (remove a conversation's storage) uses the same stop mechanics (`close_pod_terminals`, then `force_terminate_pod`) under the pod-start lock. Nothing changes here; a quiet stop leaves the claims for it to remove.
  * SME-239 (model volumes): a pod mounts `sandbox-volume-<id>` claims too. A stop unmounts them by deleting the pod and touches no volume row or claim. `delete_agent_volume_if_unused` already ignores `finished` holders, so a volume deleted while a finished child's pod still mounts it was left `Terminating` behind `pvc-protection` until that pod went; the finish stop now ends that wait.
  * SME-77, SME-121, SME-134, SME-135 and SME-255: no design change; tests that subscribe to a conversation's events start with `start_ids_clear_of_other_runs`.
  * Language-server pods carry an `ownerReference` to their sandbox pod (`src/lsp/pods.rs`), so they go with it.

## What and why

1. **On idle.** Once a pod has gone `SANDBOX_IDLE_STOP_MINUTES` (default 60) with no activity, smelt stops it the way the user's Stop does, but quietly. `/workspace`, Docker's data (images, build cache, named volumes) and mounted volumes are kept, since they're claims. Running processes, terminals, installed packages and files elsewhere are lost. The model gets one saved notice and isn't woken.
2. **At finish.** When a sub-agent's turn ends with it `finished`, smelt stops its pod at once, the same way, with its own notice. If the parent or the user writes to it again, its next `create_pod` (or `clone_repo`) starts a pod on the same claims, as today.

Activity is measured in the database, so a restart (`dx serve` restarts the dev server on every Rust edit) doesn't reset the clock.

## How comparable tools do it

Checked for the 10-09 plan and unchanged: Codespaces (30 minutes; terminal input or output resets it, a shared port alone doesn't), Daytona (15 minutes; preview requests reset it, a fire-and-forget `npm run dev` doesn't), Coder (`b27e158`, `activitybump.go`: terminal, app and port traffic and an AI chat's heartbeat bump it, written at most once per 5% of the deadline) and E2B (a timeout the caller extends). All stop on idle and keep the disk; none counts a running server on its own.

## What counts as activity

| Signal | Counts? | How it's seen |
| -- | -- | -- |
| A turn running or queued | **busy**: never stopped | `turn::turn_running`, and a stop needs the turn lock (State model) |
| A repo cloning | **busy** | `conversation_repos.status = 'cloning'` for the conversation. "Work on a repo" clones outside any turn. |
| A turn that brought something new: the user's send, the user's answer, a delivered notice (`Notice::Deliver`), an agent's start brief | yes, at the turn's **end** | the turn's `activity_owed` flag (below), noted at its end |
| A wake turn that drained an agent message (a report, a `message_agent`, smelt's notice about a child) or a finished or lost command's notice | yes, at the turn's end | the drains set the same flag |
| A wake turn for an SME-79 idle notice only, and the model's reply in it | **no** (question 2) | `drain_idle_command_notices` sets no flag |
| `terminal_commands.touched_at` (the model reading, checking, signalling or re-timing a command) | **no** (question 2) | not read. It's only written inside a turn, so a counted turn already counts; counting it alone would let a model answering idle notices keep the pod forever |
| A command starting or finishing | yes | `MAX(terminal_commands.created_at)`, `MAX(finished_at)` for the pod's terminals. A command the model starts or signals in an idle-notice turn counts through these. |
| Output from a running command | yes | the newest `terminal_events.created_at` of each running command, read through `idx_terminal_events_command_id (command_id, seq)` with `ORDER BY seq DESC LIMIT 1`, so a chatty log isn't scanned. A build that prints keeps the pod; a silent or hung command doesn't. (SME-79's notices ignore output by the user's choice; this is the pod clock, approved 10-27.) |
| A running command on its own (a dev server, a watcher, one with `idle_notice_seconds: 0`) | **only through its output and its traffic** (question 1) | Daytona's and Codespaces' rule |
| Preview traffic and pod dials | yes | each preview request (`preview.rs`'s `handle`, after the fetch guard) and each successful `sandbox::open_pod_port_with` (previews on a fresh connection, `webfetch`, browsing sessions on `localhost` and container addresses) calls `sandbox::note_activity(conversation_id)` |
| An open preview tab with no requests (an idle HMR websocket) | no | no requests, no new connections |
| The user viewing the conversation (`subscriber_count`) | **no** (approved answer 3) | a tab left open is the usual forgotten pod |
| `conversations.updated_at` | **no longer read** | it's bumped by smelt's own notices, SME-79's every 5 minutes among them |

**The turn's flag.** `ConversationRuntime` (`src/turn/state.rs`) gains `activity_owed: bool`. It's set at the start of `run_turn_bounded` when the turn has a message of its own or is the user's answer (`new_message.is_some() || keep_error.is_some()`: a send, an answer, a delivered notice; only a wake has neither), and by `drain_agent_messages` and `drain_unnotified_terminal_commands` when they save a row; `drain_idle_command_notices` doesn't set it. `run_turn_bounded`'s end (`src/turn/run.rs`, where it already spawns `after_last_turn`) takes it (`take_activity_owed`, which clears it) and calls `sandbox::note_activity`. It's taken at the end, not when set, so a 30-minute turn started by a report counts from its end. Two turns in a row share the flag; the first end takes it, which is fine, since the second is running and busy.

**`note_activity`** writes the conversation id to an in-memory set (a std mutex, never held across an await), so the preview's hot path makes no database write. The pass flushes it: `UPDATE sandbox_pods SET last_active_at = now() WHERE conversation_id = ANY($1) AND terminated_at IS NULL`, on the database's clock, at most once a minute per conversation (Coder's throttle). The set is cleared only after the update succeeds. A restart loses at most one pass of stamps.

## The threshold

* `SANDBOX_IDLE_STOP_MINUTES`: default **60**; `0` turns idle stops off; empty is unset; anything else that isn't a whole number from 1 to 10,080 (a week) logs a warning and uses 60, the pattern of `idle_notice_secs_from` (`src/sandbox/idle_notice.rs`, whose day ceiling also keeps `minutes * 60` from overflowing). Documented in `docs/setup/environment.md` beside `SANDBOX_IDLE_NOTICE_MINUTES`. The dev `.env` sets no `*IDLE*` variable (checked).
* Read once at startup. No per-conversation override.
* The pass runs every 60 s, so a pod stops between N and N+1 minutes after its last activity.
* **The finish stop doesn't depend on it:** at `0` the pass still runs for finished agents' pods (Trade-offs).

## The finish stop (2026-10-27 scope)

**Trigger.** `finish` runs inside the child's turn (`agents::queue::finish` → `db::finish_agent`, which sets `agents.state = 'finished'` and queues the report in one transaction), and `turn::reply` ends the turn after it. `run_turn_bounded`'s end then spawns `agents::after_last_turn` when the turn saved something and no other turn is running or queued. `after_last_turn` (`src/agents/queue.rs`) first reads `db::agent_state`: when it's `finished`, it calls `sandbox::stop_finished_agent_pod(pool, conversation_id)` and returns, since a finished agent isn't "ended without a report"; otherwise it goes on as today. `run.rs` is untouched for this.

**`stop_finished_agent_pod`:**

1. No live pod (`db::list_sandbox_pods` is empty): return. This is the **no-op** for a seat that already ran `terminate_pod` (every Project Manager brief's teardown step since 2026-10-26).
2. Take the conversation's turn lock and then its pod-start lock, **waiting** for each (the order a turn takes them, so it can't deadlock with one; `remove_conversation_storage` takes only the start lock).
3. Re-check in one statement (`db::pod_still_stoppable`, below): the pod is live, the agent is still `finished`, no repo is cloning.
4. Stop it quietly (below) with the finish notice.
5. Release both locks, then `release_idle_turn_state` as `save_notice_between_turns` does.

**The wake race, and its winner.** A `message_agent` to a finished child (`db::message_agent`) sets it `active` under the agent row's `FOR NO KEY UPDATE` and then wakes it; the user's send does the same through `reactivate_agent` or `resume_finished`. **Whichever takes the turn lock first wins, and the stop's re-check decides under that lock:**

* The stop gets the lock first and reads `finished`: the pod is stopped. The woken turn runs after it with no pod, reads the stop notice and the message together, and `create_pod` brings `/workspace` back. **The stop wins; the wake restarts the pod.**
* The message commits first: the wake's turn is counted at once, so either `turn_running` is already true when `run_turn_bounded`'s end checks it and no stop is spawned, or the stop's re-check reads `active` and skips. The woken turn keeps the pod.
* In both cases the invariant holds. A conversation becomes `finished` only through `finish_agent`, inside a turn whose end comes back to this check, so no finished conversation keeps a pod past its last turn's end. The only gap is a turn that doesn't reach that spawn: one stopped by the user right after `finish`, or smelt dying between the commit and the stop. The idle pass's backstop covers it.

**The backstop.** Each pass also takes every live pod of a `finished` agent that isn't busy, with no threshold. That catches the gaps above within a minute, and on the first deploy it stops the finished seats' pods already running.

**Volumes (SME-239).** The stop deletes the pod, which unmounts `/workspace`, Docker's claim and every `sandbox-volume-<id>`. It never touches `agent_volumes`, `conversation_volume_mounts` or a claim. The creator's subtree delete still governs deletion. A reawakened child's next pod mounts what its mount rows say, as any new pod does.

**Commands still running at finish** (a child that leaves a server up) are marked `lost` like any stop. Their "outcome is unknown" notices wait for the child's next turn, if there is one. Without the stop, SME-79's watch would keep waking a finished child about such a command (`agents::admits_notices` lets a `finished` agent's wakes in), and each wake runs a turn.

**Not in scope:** a child that `stop_agent` stopped keeps its pod as the user's Stop leaves it, and its pod stops on idle (Trade-offs).

## Stopping quietly (both reasons)

`src/sandbox/quiet_stop.rs` (new; declared in `src/sandbox/mod.rs` beside `idle_notice`):

* `pub(crate) enum StopReason { Idle { minutes: u32 }, Finished }`.
* `stop_locked(pool, pod_id, conversation_id, reason, terminate)`: called with both locks held after a re-check said yes.
  1. `close_pod_terminals(pool, pod_id)` (running commands `lost`, terminals closed, each published).
  2. `terminate`, which is `force_terminate_pod` in production and a fake or failing one in tests (`terminate_pod_with`'s pattern). It publishes `SandboxPodUpdate { terminated: true }` and `PodsChanged` itself.
  3. If the conversation still exists, save the notice with `db::create_message_in_place` while still holding the turn lock, so it can't split a tool call from its result, then publish `MessagesAppended`. No `ConversationsChanged`: nothing in the sidebar moved.
  4. `tracing::info!(pod_id, conversation_id, reason, "stopped a sandbox pod quietly")`, ids only.
* A failed delete (`terminate_pod_with` leaves the row live and re-registers the pod) saves no notice and logs a warning. The next pass re-evaluates the pod from scratch.

**Notices** (role `user` text, as every notice is):

* Idle: `Sandbox pod <id> was stopped after <N> minutes with no activity. /workspace, Docker's images, build cache and volumes, and mounted volumes are kept; terminals, running processes (servers included), installed packages and files elsewhere are gone. create_pod (or clone_repo) starts a new pod with /workspace as it was.`
* Finished: `Sandbox pod <id> was stopped because you finished. /workspace, Docker's images, build cache and volumes, and mounted volumes are kept; terminals, running processes, installed packages and files elsewhere are gone. If you're given more work, create_pod (or clone_repo) starts a new pod with /workspace as it was.`
* Transcript (`src/frontend/pages/chat/transcript/text.rs`, a branch on `Sandbox pod ` + ` was stopped after ` / ` was stopped because you finished`, placed before the `stopped unexpectedly` branch): "The sandbox stopped after 60 minutes idle; /workspace and Docker's data are kept" and "The sandbox stopped when this agent finished; /workspace and Docker's data are kept".

**`db::create_message_in_place`** (`src/db/conversations.rs`): `create_message_on`'s insert without the `UPDATE conversations` that bumps `updated_at` and auto-titles from a default title. A quiet notice therefore neither moves the row (approved answer 4) nor titles an untitled conversation with the notice text.

## The idle pass

`stop_idle_pods(pool)` is spawned from `main` after `watch_idle_commands` (never in the browser-test harness). `stop_idle_pods_once_with(pool, minutes, terminate)` holds the testable body. One pass:

1. Flush the activity set. Read `db::quiet_stop_candidates(minutes)`: live pods that aren't busy and are either idle past `minutes` (skipped when `minutes` is 0) or a `finished` agent's.
2. For each candidate:
   1. Skip it if `turn::turn_running`.
   2. `try_lock` the turn lock, then the start lock, and skip it if either is held. A skipped pod is tried again next pass.
   3. **A running command whose agent isn't connected** (`agent_status(pod_id)` is `None`, as after a smelt restart): call `sandbox::try_reconnect(pool, pod_id)`, which flushes the agent's queued `unsent` output, and leave the pod for the next pass. A build that printed while smelt was down isn't stopped for looking silent.
   4. Flush again, then `db::pod_still_stoppable(pod_id, reason, minutes)`. If it says no, release and move on.
   5. `stop_locked`, then release both locks and `release_idle_turn_state`.

The lock helper goes in `src/turn/outside_turn.rs`: `pub(crate) async fn between_turns<T>(conversation_id, wait: bool, f) -> Option<T>`. It takes the turn lock (waiting, or `try_lock`), runs `f`, drops the lock, then frees idle turn state (SME-219's concern: a lock taken outside a turn leaves runtime state behind otherwise). The finish stop waits; the pass doesn't.

**SQL, in `src/db/sandbox.rs`, one rule shared by `/pods`, the candidates and the re-check:**

* `POD_IDLE_SINCE`: `GREATEST(p.created_at, p.last_active_at, last command start, last command finish, newest output of a running command)`, with `GREATEST` skipping NULLs.
* `POD_BUSY`: `EXISTS (SELECT 1 FROM conversation_repos r WHERE r.conversation_id = p.conversation_id AND r.status = 'cloning')`. A running turn is in memory and checked in Rust.
* `POD_FINISHED`: `EXISTS (SELECT 1 FROM agents a WHERE a.conversation_id = p.conversation_id AND a.state = 'finished')`.
* `list_live_pods` gains `idle_since` and `busy` on `LivePodRow` and drops `conversation_updated_at`, `running_commands` and `last_command_finished_at` once `pod_activity` stops reading them (check each with grep first).
* `quiet_stop_candidates(minutes) -> Vec<(pod_id, conversation_id, StopReasonKind)>`, `pod_still_stoppable(pod_id, kind, minutes) -> bool` (both rules evaluated against `now()` in the statement), and `flush_pod_activity(&[i64])`.

`src/db/sandbox.rs` is 28,816 bytes and gains about 4,000, all production code; its tests are in `src/db/tests/pods.rs`.

## `/pods` and the model's view

* `api::pods::pod_activity` becomes `if row.busy || turn::turn_running(row.conversation_id) { Busy } else { IdleSince(row.idle_since) }`. A pod with a running command but no output or traffic now reads `idle …`, not `busy`. "idle 59m" means about a minute left.
* The footnote on `/pods` (`src/frontend/pages/pods.rs`) adds "Sandboxes idle for 60 minutes are stopped, and a finished agent's at once; /workspace is kept." or, at `0`, "Idle sandboxes aren't stopped automatically; a finished agent's is." Its text comes from a new `get_idle_stop_minutes` server function (`src/api/sandbox.rs`). It sits under the table at every width, so there's no layout change.
* **System prompt.** `src/turn/system_prompt.md`'s SME-79 bullet "…smelt never stops a command on its own." becomes "…smelt never stops a command on a timer of its own (the sandbox's idle stop, in the environment section, ends the whole pod)." `PromptEnvironment` (`src/turn/history.rs`) gains `idle_stop_minutes: Option<u32>`, and the Environment section adds, while it's on: `- Your sandbox pod is stopped after 60 minutes with no activity: your turns, commands starting or finishing, command output, and preview traffic count; the idle-command notices don't. A long silent job should print progress. /workspace, Docker's data and volumes are kept.` For an agent (`env.agent` is `Some`) it also adds `- Your sandbox pod is stopped when you finish; /workspace and volumes are kept for if you're given more work.`
* **SME-79's notice** (`drain_idle_command_notices`, `src/turn/notify.rs`): "Nothing stopped it and smelt won't stop it." becomes, while idle stops are on, "Nothing stopped it and smelt won't stop the command itself, but the whole sandbox is stopped after <N> minutes with no activity, and these notices don't count." The transcript's summary reads only up to the `:`, so it's unchanged (its test still passes on the new text).
* **Tool descriptions:** `run_terminal_command`'s `idle_notice_seconds` (`src/anthropic/tools/pods.rs`) adds "; a sandbox with no activity is still stopped after the time the environment section gives, so 0 doesn't keep a server alive", which is the redone `time_limit_seconds` edit. `finish` (`src/anthropic/tools/agents.rs`) adds "Your sandbox pod is stopped when you finish; /workspace and volumes are kept."

## State model

**The record** is one `sandbox_pods` row, keyed by `pod_id`, plus its conversation's `agents.state` for the finish rule. The states:

* **active**: live, busy or idle for less than the threshold
* **idle**: live and idle past the threshold (computed)
* **finished-live**: live, agent `finished` (computed)
* **terminated**: `terminated_at` set
* **gone**: cascaded away with its conversation

There's no stored "stopping" state: the re-check under both locks replaces the old plan's `idle_stopped_at` claim, and a stop that fails partway is re-evaluated on the next pass.

| State \ event | Activity (counted turn ends, command, output, preview, dial) | Idle pass | `finish` turn ends | `message_agent` / user send to the child | Turn starts | User Stop / crash / watch close | Conversation deleted | smelt restarts | `create_pod` / `start_or_get_pod` |
| -- | -- | -- | -- | -- | -- | -- | -- | -- | -- |
| active | stays active | not a candidate | → finished-live → finish stop | stays active | busy | terminated (own notice) | gone (teardown) | unchanged (clock in DB) | `create_pod` refused; `start_or_get` returns it |
| idle | → active (next flush, or the re-check's) | locks, re-check, stop → terminated; a lock held → skipped | (only from active) | its turn → active | busy; skipped | terminated | gone | first pass stops it | the start lock makes the pass skip; the clone is busy |
| finished-live | ignored (no clock) | backstop: locks, re-check, stop | the finish stop waits for the locks, re-checks, stops | `active` → the re-check skips; after a stop, the woken turn finds no pod | busy; the stop waits | terminated; the finish stop no-ops | gone; re-check finds no row | backstop stops it | the stop waits for the start lock; a clone is busy → skip |
| terminated | flush matches nothing | not a candidate | no-op | – | – | `NoPod` | gone | – | a new row, active |

**A repeat of the creating event:** a second `finish` in one turn is refused by `finish_agent` (`NotActive(finished)`). A finish stop and the backstop pass on the same pod: the pass's `try_lock` fails while the finish stop holds the locks, or its re-check finds the row terminated. Two finish stops (two turn ends in a row): the second finds no live pod.

**Writers of the row:**

* `create_sandbox_pod`
* `terminate_sandbox_pod`, through `terminate_pod`, `stop_pod_for_user`, crash cleanup, the watch's `close_if_gone`, a failed start's rollback, `remove_conversation_storage`, and now `stop_locked`
* `flush_pod_activity` (`last_active_at`, new; its only writer)
* the conversation delete's cascade

**Writers of the activity inputs:**

* the turn's `activity_owed` flag: `run_turn_bounded`'s start (a send, an answer, a delivered notice), `drain_agent_messages`, `drain_unnotified_terminal_commands`; taken at `run_turn_bounded`'s end
* `run_command`'s insert, the agent's `Exit` handler (`finished_at`), and `close_pod_terminals` (`lost`, no `finished_at`, so a stop isn't activity)
* the agent's `Output` handler (`terminal_events`)
* the clone status writers
* `note_activity`'s callers: preview `handle`, `open_pod_port_with`, and the turn end
* for the finish rule, `agents.state`: `finish_agent` (→ finished); `message_agent`, `reactivate_agent`, `reactivate_finished` (→ active); `stop_subtree` (→ stopped, which takes it out of the rule)

**Readers:** `/pods` (`list_live_pods`), the candidates, the re-check, a terminal or file tool resolving the pod (`conversation_pod_id`), the preview's dial, `after_last_turn`'s state read, and `delete_agent_volume_if_unused`'s holder query (unchanged; finished holders didn't block before either).

**Between the check and the act:**

* *Candidate read → re-check:* the activity in between is flushed before the re-check, which is one statement.
* *Re-check → delete:* a turn can't start (the turn lock), and neither can a pod start, a clone's included (the start lock). What can still happen in those few hundred milliseconds:
  * A preview request or a browsing load dials the pod and fails as it goes. That needs a request at exactly minute 60 after an hour with none.
  * A running command prints, and its output is lost with it.
  * `message_agent` sets a finished child `active`: the stop goes ahead and the woken turn finds no pod (the stop wins).
  * The user's Stop: both tear down; each is idempotent, and two notices are possible.
* *The conversation is deleted after the re-check:* the delete doesn't take the turn lock. Teardown and our delete each tolerate the other (404 is Ok). `create_message_in_place` would fail on the foreign key, so `stop_locked` checks `conversation_exists` first and treats a foreign-key error as gone (logged at debug).

**Failure part-way:**

* `close_pod_terminals` fails part-way: it's best-effort per terminal, as today. The commands it did mark stay `lost`, and the next pass re-evaluates a pod that still has no running command.
* The delete fails: the row stays live (re-registered), no notice is saved, and the next pass re-checks it. A finished child woken in between keeps its pod, with its terminals closed.
* smelt dies between the delete and the notice: no notice (flagged). The model still gets `no such pod` and the lost-command notices.
* The flush fails: logged, and the set is kept for the next pass.
* The re-check fails (a database error): logged, released, skipped.

**A pod restart (crash):** the crash path terminates the row and notices, as today. Nothing of this feature lives in the pod.

**Work already in flight:** the threshold is read once at startup, and a restart applies a new value to every live pod at the next pass, the time already idle included; the notice gives the minutes in force. A running turn is never interrupted. A running command ends only through a stop the rules above allow, and is then `lost`, as with any user Stop.

**What each caller does in each state:**

| Caller | active | idle | finished-live | terminated |
| -- | -- | -- | -- | -- |
| model's terminal or file tool | works | works (its turn makes the pod active) | works in a woken turn (the re-check then skips) | `no such pod` |
| `create_pod` | refused (exists) | refused (exists) | refused (exists) | creates on the same claims |
| `start_or_get_pod` (clone) | returns it | returns it; the pass skips while it holds the start lock | returns it; the finish stop waits for the lock, then the re-check sees `cloning` and skips | creates |
| preview / browsing dial | connects, notes | connects, notes → active | connects (doesn't save it) | "no running sandbox" page / refused |
| user Stop | stops | stops | stops; the finish stop then no-ops | `NoPod` |
| `/pods` | busy or idle Xm | idle ≥ threshold, under a minute | idle, for seconds | not listed |
| idle pass | skip | stop | stop (backstop) | skip |
| finish stop | skip (re-check: not finished) | skip (re-check: not finished) | stop | no-op |
| SME-79 watch | notices a running command | notices it; the notice doesn't count | notices it (a wake for a finished agent, as today) until the stop | nothing running |
| `remove_conversation_storage` | stops it itself | the same; it holds the start lock, so the pass skips | the same | removes the claims |

## Files

Checked against `git ls-files` at `e3869e4`. Sizes are now, against the 40,000-byte ceiling.

**New:**

* `migrations/20261031120000_sandbox_pod_last_active.sql`: `ALTER TABLE sandbox_pods ADD COLUMN last_active_at TIMESTAMP NULL`. The newest on main is `20261028120000_named_secrets.sql`, and PR #199 adds none. Recheck at commit time.
* `src/sandbox/quiet_stop.rs`: `note_activity`, `idle_stop_minutes`/`idle_stop_minutes_from`, `stop_idle_pods`, `stop_idle_pods_once_with`, `stop_finished_agent_pod`(`_with`), `stop_locked`, `StopReason`.
* `src/sandbox/tests/quiet_stop.rs`: the pass's and the stop's `#[sqlx::test]`s, plus the real-pod test (declared in `src/sandbox/tests/mod.rs`, 9,256).
* `src/agents/tests/finish_stop.rs`: the finish trigger and its races (declared in `src/agents/tests/mod.rs`, 8,683).
* `src/turn/tests/pod_activity.rs`: the flag and the prompt lines (declared in `src/turn/tests/mod.rs`, 13,612). They go here and not in `prompt.rs` (28,274) or `wake.rs` (23,633), so those stay under the 30,000-byte aim.
* `src/api/pods/tests.rs`: **commit 1 is a move only.** `src/api/pods.rs` is 35,923 bytes with its tests in-module; they move out (`#[cfg(test)] #[path = "pods/tests.rs"] mod tests;` or `mod tests;` with `pods/` as the module's directory), checked by a build of every target.
* `docs/current-state/sandbox.md`: **commit 2 is a docs move.** `docs/current-state/features.md` is 38,346 bytes, inside 2,000 of the ceiling, so its sandbox bullets (the ones starting "See and stop sandbox pods", "The sandbox pod has a configurable memory limit", and the other sandbox lines) move to a page of their own, linked from `features.md` and `docs/current-state/index.md`. PR #199 edits `features.md` line 34; if it merges first, cut the move from its version.

**Changed:**

| File | Bytes | What |
| -- | -- | -- |
| `src/db/sandbox.rs` | 28,816 | the three fragments, `LivePodRow`, candidates, re-check, flush |
| `src/db/conversations.rs` | 9,695 | `create_message_in_place` |
| `src/db/tests/pods.rs` | 8,598 | the SQL tests |
| `src/db/tests/conversations.rs` | 10,995 | `create_message_in_place` leaves `updated_at` and the title |
| `src/api/pods.rs` | 35,923 → ~27,000 after the move | `pod_activity` |
| `src/api/sandbox.rs` | 7,915 | `get_idle_stop_minutes` |
| `src/frontend/pages/pods.rs` | 19,008 | the footnote |
| `src/frontend/pages/chat/transcript/text.rs` | 24,093 | the two summaries |
| `src/frontend/pages/chat/tests/notices.rs` | 22,770 | their tests |
| `src/turn/state.rs` | 26,344 | `activity_owed`, `set_activity_owed`, `take_activity_owed` |
| `src/turn/run.rs` | 30,746 | about 5 lines in `run_turn_bounded`: set the flag at its start, take it at its end |
| `src/turn/notify.rs` | 16,972 | the flag in two drains; SME-79's sentence |
| `src/turn/outside_turn.rs` | 5,545 | `between_turns` |
| `src/turn/history.rs` | 22,943 | `idle_stop_minutes`, the Environment lines |
| `src/turn/system_prompt.md` | 11,055 | the reworded bullet |
| `src/agents/queue.rs` | 7,202 | `after_last_turn` → `stop_finished_agent_pod` |
| `src/anthropic/tools/agents.rs` | 10,036 | `finish`'s description |
| `src/anthropic/tools/pods.rs` | 31,721 | one clause in `idle_notice_seconds`; no tests added |
| `src/sandbox/mod.rs` | 18,123 | `mod quiet_stop; pub(crate) use self::quiet_stop::*;` |
| `src/sandbox/ports.rs` | 7,038 | `note_activity` once the pod resolves (not on `NoPod`) |
| `src/preview.rs` | 30,917 | `note_activity(conversation)` in `handle` after the guard; about 2 lines |
| `src/preview/tests.rs` | 12,497 | its test |
| `src/main.rs` | 17,335 | spawn `sandbox::stop_idle_pods` after `watch_idle_commands` |
| docs | | `docs/setup/environment.md` (16,284: the variable), `docs/database.md` (29,605: the column), `docs/api/events.md` (30,777: line 123's Activity bullet), `docs/api/agents.md` (18,867: finish stops the pod, the race), `docs/architecture/modules-sandbox.md` (26,347: a `quiet_stop.rs` row, the `pods.rs` row), `docs/architecture/modules-core.md` (19,589: startup order), `docs/current-state/index.md` (4,661: SME-78 out of the open goals), `docs/current-state/agents.md` (7,606), `docs/feature-checklist/sandbox.md` (15,413: rows and the hands-on check) |

**Not touched:** the agent sources (`src/bin/sandbox_agent/`, `src/agent_protocol.rs`, `src/docker_net.rs`, `Cargo.toml`), so no image rebuild; `src/sandbox/manager/*`, `src/db/agents/*`, `src/sandbox/idle_notice.rs`.

**Callers that gain a need, by grep:** the `LivePodRow { … }` literals (the moved `api/pods` tests' `row()` helper, and any other `grep -rn "LivePodRow {" src`); `PromptEnvironment { … }` literals (`grep -rn "PromptEnvironment {" src`, `#[ignore]`d tests included); every reader of `conversation_updated_at`, `running_commands` and `last_command_finished_at` before they're dropped.

## Tests (each seen red first; actual red recorded beside each prediction)

* **db** (`src/db/tests/pods.rs`): each counted signal moves `idle_since` (pod start, `last_active_at`, command start and finish, a running command's newest output); a finished command's later events don't count past its `finished_at`; **a new message doesn't move it** (the SME-79 case; expected red today, since `pod_activity` reads `conversation_updated_at`); a cloning repo is busy and a running command isn't; candidates are idle past the threshold or a finished agent's at any age, never busy or terminated, and no idle ones at `minutes = 0`; `pod_still_stoppable` turns false after fresh activity, a clone starting, the agent going `active`, or a terminate; the flush writes only live pods. `src/db/tests/conversations.rs`: `create_message_in_place` leaves `updated_at` and a default title alone.
* **turn** (`src/turn/tests/pod_activity.rs`): a user's turn notes activity at its end, not its start (a hook holds it mid-turn; expected red: there's no flag yet); a wake that drains only an idle notice notes nothing; one that drains an agent message or a finished command's notice does; the Environment lines (with the minutes, absent at `0`, the agent line only for an agent); SME-79's sentence, new while on and old at `0`.
* **quiet stop** (`src/sandbox/tests/quiet_stop.rs`, fake terminate, back-dated timestamps): an idle pod is stopped (commands `lost`, terminals closed, one notice, `updated_at` unchanged, `SandboxPodUpdate { terminated: true }` and `PodsChanged`); skipped while `turn_running`, its turn lock or its start lock is held, and stopped next pass once free; activity noted between the candidate read and the re-check saves it; a failing delete saves no notice and the next pass stops it with exactly one; a conversation deleted after the re-check ends quietly; a user Stop first leaves nothing to do; at `0` an idle pod stays and a finished agent's still goes; env parsing (unset or empty → 60, `0` → off, `10081`, `-1`, junk → 60 with a warning); a running command on a disconnected agent reconnects and defers.
* **finish** (`src/agents/tests/finish_stop.rs`, the agents tests' fake model): a child's `finish` stops its pod at the turn's end with the finish notice, its volume and mount rows untouched (expected red: `after_last_turn` returns without a stop); no pod (it ran `terminate_pod`) → no stop, notice or error; **stop first**: the test holds the child's turn lock, the stop queues on it, `message_agent` lands, release; the woken turn finds no pod, with the notice then the message in its history; **wake first**: `message_agent` commits before the turn's end, so no stop is spawned and the pod stays, and a wake landing between the spawn and the re-check reads `active` and skips; a turn stopped right after `finish` leaves the pod to the next pass, which stops it; a `stop_agent`-stopped child keeps its pod.
* **sources and views:** a preview request notes its conversation (`src/preview/tests.rs`); `open_pod_port_with` notes on success, not on `NoPod` (the stand-in client tests in `src/sandbox/tests/guards.rs`); `pod_activity` (`src/api/pods/tests.rs`); both transcript summaries, and SME-79's unchanged on its new sentence (`notices.rs`).
* **real pod** (`quiet_stop.rs`, under the cluster lock): a pod with `/workspace/kept.txt`, a pulled small image and `sleep 600`; back-date and run one pass with the real delete; the pod is gone, both claims stay, the command is `lost`, the notice saved; `create_pod` brings back `kept.txt` and the image (`docker image inspect`).

**Which part each test needs:** the db "a message doesn't count" test needs only the SQL change. The turn tests need the flag. The finish tests need `after_last_turn`'s branch, and their race variants also need the waiting `between_turns`. A `try_lock`-only version fails "stop first". The pass's backstop test needs the candidates' finished clause, and fails with only the turn-end trigger in. The Developer checks each claim against a build with that part stubbed out.

**Commits:** 1 the `api/pods` test move; 2 the `features.md` split; then the migration with the SQL and its tests; `note_activity` with its first callers (preview, dial) in the same commit as the pass that flushes it, since a set nobody flushes is dead code; the turn flag; the finish stop; the texts and docs. `stop_locked` has no caller until the pass or the finish stop exists, so it lands with the pass.

## Hands-on check

A scratch database through `scripts/check-server`, from the rules' template file (`<scratchpad>/sme-78/hands-on.sh`), with `-- SANDBOX_IDLE_STOP_MINUTES=3 SANDBOX_IDLE_NOTICE_MINUTES=1` and previews moved off 8181 (rules.md). The floor is 1 minute plus a 60 s pass; 3 minutes leaves room to see SME-79's notices not counting.

1. Ask the model to write `/workspace/kept.txt` and start `python3 -m http.server 8000`, then share a preview.
2. Reload the preview every 30 s for 5 minutes: the pod stays up, though an idle notice arrives every minute.
3. Stop reloading. Within about 4 minutes:
   * the transcript shows the idle line;
   * the sidebar dot and the `/pods` row go;
   * the conversation doesn't move in the sidebar;
   * the preview shows its no-sandbox page.
4. Ask the model to `cat /workspace/kept.txt`: it creates a pod and reads it.
5. Finish: ask the model to start an agent whose brief is "create a pod, write /workspace/child.txt, then finish". Within seconds of its report, its `/pods` row goes and its transcript shows the finish line. `message_agent` it "cat /workspace/child.txt": it creates a pod and reads the file.

Delete the check's conversations in the check server's UI before `stop`.
