# SME-236: child agents — state model, commits and tests

The second half of [the plan](sme-236.md).

## State model

### `agents.state` × event

| State ↓ / event → | start done | start fails | `stop_agent` (on it or an ancestor) | `finish` | user's send / answer | wake / delivered notice | user's Stop | it is deleted | its parent is deleted | restart (sweep) | its turn ends |
| -- | -- | -- | -- | -- | -- | -- | -- | -- | -- | -- | -- |
| starting | → active, start notice to itself, wake | → failed, report to parent | → stopped (`started_at` NULL); the start's final update finds 0 rows, `StartExit` cleans up | not offered a turn | refused "still starting" | wake: no-op; notice: saved only | refused | notice to parent; rows gone; the start's writes fail → `StartExit` | rows gone | → failed, report "smelt restarted during the start", cards deleted | — (no turn) |
| active | — | — | → stopped, turns stopped | → finished, report queued, turn ends | turn runs | turn runs | paused (memory); notice to parent | notice to parent | rows gone, turns stopped | turn lost; undelivered rows woken | notice to parent if nothing else pending (above) |
| finished | — | — | → stopped | refused ("already finished") | → active, turn runs | turn runs (state stays finished; the drain writes no state) | paused, no notice (not active) | notice | gone | woken if rows | no notice (not active) |
| stopped | — | — | already (no change) | refused ("stopped") | started: → active; never started: refused | turn runs if not paused: the stop paused it in memory, so a wake is refused until the user writes | already paused | notice | gone | woken if rows (the pause is lost: SME-238 stores it) | no notice |
| failed | — | — | no change | not reachable (no turn) | refused "start failed" | nothing is queued or delivered to it | allowed, harmless | notice | gone | unchanged | — |

A repeat of an event in the state it made: a second `finish` in `finished` is refused; a second `stop_agent` reports "already stopped"; a second start can't happen (one task per child, and the sweep only fails rows still `starting`).

### `agent_messages` rows

States: undelivered → delivered (only the drain, under the recipient's turn lock, in one transaction with its message). Gone with its recipient (cascade); a deleted sender leaves `from_conversation_id` NULL and `from_label` kept. **Every writer:** `finish` (report), `fail_start` (report), the sweep's failure (report), `after_last_turn` (notice), `user_stopped` (notice), the start's done step (notice to the child itself), the start's unresolved model (notice), the subtree delete (notice to the deleted child's parent). Each inserts in a transaction with the state change it reports, so one lands only if the other does, and each wakes the recipient after commit. **Readers:** the drain, `wake_has_work`, the startup sweep. A drain stopped between its rows: each row is its own transaction, so delivered rows stay delivered and the rest wait (SME-91's rule). A drain failing after its `UPDATE … delivered_at` rolls back with the message.

### Lock order (every transaction this ticket adds or changes)

(1) the remote's trust advisory lock, if any; (2) `conversations` rows in ascending id, in the mode given; (3) everything else. Existing writers already in this order: the trust decision (SME-181: trust lock, then `FOR KEY SHARE` on conversations in id order); `record_model_call_if_current` (one row, `FOR NO KEY UPDATE`).

| Transaction | Locks, in order |
| -- | -- |
| `start_agent` | trust lock; caller `FOR NO KEY UPDATE`; inserts (the new row's FK takes `KEY SHARE` on the caller, already held stronger) |
| start done / `fail_start` | parent `FOR KEY SHARE`, child `FOR NO KEY UPDATE` (its model may change); `agents`; insert |
| `finish` | parent, child `FOR KEY SHARE`; `agents`; insert |
| `stop_agent` | subtree `FOR KEY SHARE` in id order; `agents` |
| drain | recipient `FOR NO KEY UPDATE`; `agent_messages`; `messages` |
| delete A | root `FOR NO KEY UPDATE`; `conversations` (deleting_at, same rows' later update is under the root's lock only); `agents` |
| delete B | parent `FOR KEY SHARE`, subtree `FOR UPDATE`, ascending; insert; `DELETE` (cascade) |
| sweep | per child: as `fail_start` |

Why it matters: without (2) first, a `finish` holding the child's `agents` row and waiting on its parent's row for the report's foreign key deadlocks with a delete of the parent holding the parent's row and cascading into that `agents` row (SME-181's lesson: an `ON DELETE CASCADE` locks child rows in physical order). Postgres would abort one with a deadlock error; with the order, one waits for the other. `FOR NO KEY UPDATE` rather than the design's `FOR UPDATE` for the root in `start_agent` and delete A: the two still exclude each other, and neither then blocks the trust decision's `FOR KEY SHARE` on the same row (the docs commit updates agents.md).

### Who calls what, in each state (admission)

| Caller ↓ / child state → | starting | active | finished | stopped (started) | stopped (never started) | failed |
| -- | -- | -- | -- | -- | -- | -- |
| user's send | refused | turn | → active, turn | → active, turn | refused | refused |
| user's answer (card) | (no card) | turn | → active, turn | → active, turn | refused | refused |
| wake (`Notice::Wake`) | no-op | turn if work | turn if work | refused while paused | refused while paused | turn if work (none is queued to it) |
| delivered notice (`Notice::Deliver`) | saved, no turn | turn | turn | saved (paused) | saved (paused) | turn (no producer sends one) |
| user's Stop | refused | paused + notice | paused | paused | paused | paused |
| `stop_agent` | → stopped | → stopped | → stopped | no change | no change | no change |
| `finish` (its own) | — | → finished | refused | refused | — | — |

### Check-then-act

- `start_agent` checks the parent's commit and persona in its pod, then writes: the parent may commit or switch branch in between. The child gets the commit read, which was on its upstream when read; the persona is read again at the child's ref (step 4), so a persona removed meanwhile fails the start instead of running without it.
- `start_agent`'s trust check and its `cloning` row are one transaction under the trust lock; the start re-reads trust after the clone (a Forget meanwhile fails the start), and `request_or_load` reads it again under the same lock.
- The start's done step, `fail_start` and `finish` each change state with `WHERE state = <expected>` in the transaction that queues their message, so a `stop_agent` or delete between a check and the act makes the act a no-op, not a second state.
- `admit_user_turn` reads and updates the state before `start_by_user`: a `stop_agent` committing after the update stops the turn the admission counted (`stop_conversation` after the update, SME-216's order).
- The "ended without a report" check runs after the last `TurnInFlight` is dropped, in one query; a command finishing or a message arriving just after it wakes the child again, which is fine: the notice says the turn ended, not the work.
- `wake_conversation`'s `starting` check is before its admission; a start completing between them only skips a wake whose work the start's own wake drains.

### Restarts

Turn state is in memory: a restart loses turns in flight and the user's Stop pause. The sweep (`agents::startup_sweep`, spawned in `main` after `fail_unfinished_clones`): every child still `starting` → `fail_start("smelt restarted during the start")`, plus `drop_start_credentials` against its pod if live, and its cards deleted; then `Notice::Wake` for every conversation with undelivered `agent_messages` whose `agents.state` isn't `stopped` (no row counts as not stopped). Out of scope here (SME-238, with `paused_at`): a conversation the user had Stopped is woken by the sweep if it has undelivered rows, and a child cut off mid-turn gets no "carry on" notice.

### In flight

The persona is a snapshot: a later edit to the file, by the child or a pull, changes nothing. The user changing a child's model applies from its next turn, as for any conversation (SME-72). A `stop_agent` while the start clones doesn't cancel the clone; the start finishes its current step and stops at its done step.

## Commits and tests

Each commit passes `scripts/check.sh` on its own: no item without a non-test caller (a dead-code warning fails the build). Every test is seen red first; "expected:" lines are predictions, and the Developer records the actual red next to each.

1. **Move `run.rs`'s request, reply and tool-running helpers to `src/turn/reply.rs`** (a move only; a build of every target).
2. **Move `chat.rs`'s tests to `src/api/chat/tests.rs`** (a move only).
3. **A root starts a plain child, which runs its brief.** Migration; `db/agents.rs` (create, state, deliver); `Conversation.parent_id`; `Tool.offer` and `tool_definitions(pool, id)`; `start_agent` without `repo`; the start (no pod steps: straight to done); `drain_agent_messages`; `wake_has_work`; `admit_user_turn`'s `starting` refusal (needed: the child exists while starting); the child's Environment lines. Tests: a root is offered `start_agent` and not `finish`, a child the reverse, and each call is refused from the other (expected red: the call runs, since `execute` dispatches by name only); `start_agent` returns the id at once, the child has `parent_id`, an `agents` row `starting`→`active`, the brief as its first message and the start notice as its second, and its first turn calls the mock upstream once; the child's model is its parent's; a send to a `starting` child is refused (a fake checkout holding the start); `wake_has_work` sees an undelivered row; `start_agent` from a caller with `deleting_at` is refused.
4. **`finish` and the report.** `finish`; `run_tools`' "ended". Tests: the report lands in the parent's transcript as "Report from agent #N:" and wakes it (one mock call); a busy parent (a mock upstream holding the parent's turn mid-call) takes it at its next iteration, in the same turn; two children finishing at once arrive in one parent turn, in `id` order; `finish` twice is refused the second time; the child's turn ends after `finish` without another model call.
5. **`list_agents`.** Tests: only the caller's children, never another root's; each status (starting via a held fake, running, waiting for the user, finished, idle, stopped, failed); `agents_started`.
6. **`stop_agent` and the user's admission.** Tests: refuses another root's child and a non-child id; stops the subtree (a grandchild row made directly in the database, since depth 1 can't make one through the tool) and ends a running child's turn; a `starting` child stopped during its start never becomes `active` (the held fake released after the stop) and a user's send in it is refused; a `finished` or `stopped` child the user writes in becomes `active`; a `failed` one is refused; the user's Stop on a `starting` child is refused, and on an `active` one queues the parent's notice once (a second Stop adds none).
7. **The "ended without a report" notice.** Tests: a child's turn ending with plain text gives the parent the notice with the first 500 characters; none when it asked the user, has a command running, finished, was stopped by the user, or is a user conversation; a failed turn's notice carries the failure.
8. **The startup sweep and a failed start.** `fail_start`, `StartExit`, the sweep. Tests: a `starting` child is failed by the sweep with "smelt restarted during the start" queued to the parent and its card deleted; a queued report survives a "restart" (turn state forgotten, the sweep run) and wakes the parent; a `stopped` recipient isn't woken; `StartExit` runs on done, on failure and when the child is deleted mid-start (test hook count).
9. **`repo` with an explicit `ref`.** `ChildCheckout`/`PodCheckout`, `clone_child_repo`, `checkout_ref`, trust at `start_agent` and after the clone, the AGENTS.md load. Tests (fake checkout): an undecided and a declined remote are refused before anything is written; the child's repo row is `cloning` then `ready`; trust Forgotten after the clone fails the start (report, `failed`, no card left); a trust card made by the load (Forget landing between the re-read and the load, via a test hook) is deleted; a clone failure is the report. Real cluster (`src/sandbox/tests/child_checkout.rs`, through `scripts/cluster-test`): `checkout_ref` at a branch (tracking), a tag (detached), a commit id not at a tip, an unknown ref (refused).
10. **The default ref: the parent's exact commit.** `child_ref::parent_commit`. Real cluster: a clean parent behind its upstream gives its commit and the child is on a branch of the upstream's name at it; a detached HEAD, a branch with no upstream, one ahead of it, and uncommitted changes to a tracked file are each refused with their message; an untracked file is not refused. Fake: the child's clone gets `Commit { sha, branch }` from the parent.
11. **`persona`.** The parent-now check, the read at the child's ref, `links::absolute`, the snapshot, `# Persona`, the model. Tests: a name not in the parent's checkout is refused before anything is written; with a `ref`, a persona only at that ref is used; missing or unparsable at the child's ref → `failed` with the parse error as the report; the system prompt has `# Persona` before Project instructions with rewritten links; changing the file after the start changes nothing (the snapshot is read from the row); `smelt.model` naming a model on another provider picks it; one no provider has keeps the parent's and notifies the parent. Real cluster: the persona read from a child cloned at a ref where the parent's copy differs.
12. **Deleting a subtree.** `delete_subtree`, the sidebar label, the browser scenario. Tests: deleting a root deletes its children and tears down each one's pods (a fake teardown hook records the ids) and stops their turns; deleting a child queues the parent's notice, and a failed delete (a test hook erroring after the notice's insert) leaves neither; a `start_agent` racing a delete (paused after delete A's commit) is refused; a `finish` racing a parent delete completes or is refused without a deadlock error (both orders, test hooks). Browser: a root with two children shows "Delete + 2 agents?" when armed, no overflow at 390px and 1400px.
13. **Docs**: database.md, models.md, api.md, architecture.md, Current state, the Feature checklist row, the design page notes.

Which part each test needs: commit 3's offering tests fail on the unfixed registry for the right reason only if `execute` would otherwise run the tool, so each asserts the refusal text, not just an error. Commit 4's "busy parent" test needs the drain in the loop, not only at turn start: with the drain stubbed to run only on the first iteration it must fail (the Developer checks with that stub). Commit 12's race tests each need their lock: with delete B's id-order locks removed, the `finish` race must show the deadlock error (checked with the lock stubbed out).

## How it will be checked by hand

On a check server (`CHECK_SCRATCH_DB=1 scripts/check-server start`, from the rules' template file, `<scratchpad>/sme-236/hands-on.sh`), with a llama.cpp or Anthropic provider added through the UI:

1. A new conversation: "Start an agent with the brief: write hello.txt containing hi and report its sha256." The child appears in the sidebar, runs, calls `finish`; the parent's transcript gets "Report from agent #N" and the parent answers.
2. "Work on a repo" with `https://github.com/cconstantine/smelt` (trust it), then ask the parent to start an agent on `/workspace/smelt` with persona `…` (once SME-247's files exist; until then a scratch fork with one persona file) and check the child's context view shows `# Persona` and AGENTS.md.
3. Ask for a child on a branch that doesn't exist: the report says the start failed.
4. Stop a running child from the sidebar: the parent gets the notice. Delete the parent: the Delete says "+ N agents", and `/pods` shows nothing left.

## Trade-offs to flag

- **The PR is large** (13 commits, most of a new `src/agents/` module, about 60 KB of code and tests). Splitting off the subtree delete was considered and kept in (see Decisions): the teardown is required once children exist.
- **A parent's `git fetch` runs in its pod** when `start_agent` takes the default ref: it updates the parent's remote-tracking refs, as any fetch does, and takes as long as the fetch (bounded by `timeout`, 60 s).
- **After a restart, a conversation the user had Stopped is woken if a report is queued to it**, and a child cut off mid-turn waits for its parent or the user rather than carrying on: both need the stored pause, which is SME-238's (`paused_at`).
- **A report's text arrives as a `user` message** naming its sender; the transcript doesn't style it as from an agent yet (the `delivered_message_id` link is there for SME-246/SME-238 to use).
- **Children show in the sidebar as plain conversations**, titled by their brief's first 60 characters, until SME-246's tree.
- **`stop_agent` doesn't cancel a clone in progress**; the start notices the stop at its last step, so a child stopped mid-clone finishes cloning (up to `CLONE_TIMEOUT`) before it ends.
