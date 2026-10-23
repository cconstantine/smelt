# Child agents

Part of [api.md](../api.md). How a conversation's model starts agents, hears from them and stops them (SME-236). The design is [docs/design/personas/agents.md](../design/personas/agents.md); the code is `src/agents/` (the lifecycle), `src/anthropic/tools/agents.rs` (the tools), `src/db/agents/` (the rows) and `src/git/server/child_ref.rs` (the child's checkout).

A child agent is a whole conversation with a `parent_id`: its own transcript, model, streaming, Stop, usage and sandbox, which the user can open and write in. Its lifecycle is `agents.state`: `starting`, `active`, `finished`, `stopped` or `failed` (see [database.md](../database.md)).

## The tools

Four tools, at the end of the registry so every older tool keeps its place in the request. A tool's `Offer` says who is given it (`tool_definitions(pool, conversation_id)` leaves the others out) and `execute` checks it again from the database, so a call from an older history is refused too.

| Tool | Offered to | What it does |
| -- | -- | -- |
| `start_agent(brief, repo?, ref?, persona?)` | a conversation without a parent (so agents are one level deep until SME-245) | Records the child (`starting`), its brief as its first message and, with `repo`, its copy of that checkout as `cloning`, in one transaction; spawns its start and returns the child's id at once. |
| `list_agents()` | every conversation | Its own children only, newest first: id, title, persona (`<dir>:<name>`), status, when it started, its last message, how many agents it started. Status is the first that holds of `starting`, `failed`, `stopped`, `running` (a turn in flight), `waiting for the user` (an unanswered question or an AGENTS.md card), `finished`, `idle`. |
| `stop_agent(id)` | every conversation | One of its own children and its whole subtree set `stopped` (state first), then each one's turns stopped (`turn::stop_conversation`). Pods and terminals are left alone. Any other id is refused. |
| `finish(report)` | an agent | The child `finished` and its report queued to its parent in one transaction, then the parent woken. A successful `finish` ends the child's turn as `ask_user` does (`run_tools`). A second one is refused. |

**`start_agent`'s checks**, all before anything is written: `brief` 1–30,000 characters; `repo` a `/workspace/<dir>` that is one of the caller's `ready` checkouts; `ref` a plain ref (`git::validate_ref`: no whitespace or control characters, no `..` or `@{`, not starting with `-`, not ending in `/` or `.lock`); `persona` a valid persona name, needing `repo`. Without `ref`, the parent's checkout must be on a commit its upstream has (after a `git fetch` in the parent's pod) with no uncommitted changes to tracked files (`git::parent_commit`; a detached HEAD, no upstream, unpushed commits or a dirty tree is refused, saying what to do), and the persona must be in it now. In the transaction: the remote's trust lock, then the caller's row `FOR NO KEY UPDATE`; the caller must exist, not be `deleting_at`, not be an agent itself, and the remote must be trusted (`repo_trust.trusted`; undecided or declined is refused).

## The start

A spawned task (`agents::run_start`) per child, holding `StartExit`, whose cleanup runs on every way out (done, failed, stopped, deleted, the task dropped): it deletes any AGENTS.md trust card in the child and runs the credentials exit SME-242 fills. With a repo, its pod work goes through `ChildCheckout` (`PodCheckout` in production; tests register a fake per parent):

1. **Clone** (`git::clone_child_repo`): the default branch into the child's pod, then `checkout_ref`: a branch is checked out tracking the remote's, a tag or a commit id detached, a commit no ref reaches fetched by id; without a `ref`, the parent's commit on a branch named as its upstream.
2. **Trust again**, under the trust lock: a remote forgotten while it cloned fails the start.
3. **AGENTS.md**: the checkout's top-level one is loaded (`request_or_load`); a card instead (the remote forgotten since step 2) fails the start.
4. **Persona**: `.agents/personas/<name>.md` read in the child's checkout (symlink-safe), parsed, its relative links made absolute (`personas::links::absolute`), and kept as a snapshot.
5. **Model**: the persona's `smelt.model` on the first provider that has it (the parent's, the default's, then by name); none has it, the child keeps its parent's model and the parent is told.
6. **Done** (`db::complete_start`): `active`, `started_at`, the snapshot and the model, and a notice to the child itself, which its first turn drains; then `Notice::Wake`. A child no longer `starting` (stopped or deleted meanwhile) is left alone.

A step that fails ends the start through `fail_start`: the child `failed`, the error queued to the parent as its report and saved in the child's own transcript. Nothing makes a failed child `active` again.

No turn runs in a child while it's `starting`: the user's send or answer is refused ("This agent is still starting."), a wake is a no-op and a delivered notice is only saved (`admits_notices`). A `stopped` or `failed` child is treated the same way for wakes and notices, so it stays quiet even after a restart loses the Stop's pause (review 1). The user's send or answer in a `finished` or `stopped` child that had started makes it `active` (`admit_user_turn`); one in a `failed` child, or one stopped during its start, is refused.

## The queue

Reports and notices reach a conversation through `agent_messages`, never `Notice::Deliver`: `drain_agent_messages` runs beside the two command drains at every iteration of `run_turn_steps`, so a busy parent takes a report between a tool result and its next call, and several arrive in one model call, in the order they were queued. Each row is saved as one `user` message (`Report from agent #12 (smelt:developer):` + the report, or `Notice: …`) and marked delivered in one transaction (`db::deliver_agent_message`). `db::wake_has_work` counts undelivered rows. Every writer queues in the transaction of the state change it reports, then wakes the recipient (`Notice::Wake`):

- `finish` (report), a failed start (report), the start's done step (a notice to the child itself; and to the parent when the persona's model wasn't found);
- the user's Stop on an `active` child not already paused (`agents::user_stop`, a notice to its parent); a Stop on a `starting` child is refused;
- a child's last turn ending while it's `active` with nothing to wait on (no unanswered question, card, running command or live agent of its own) and not paused (`agents::after_last_turn`, a notice with the first 500 characters of its last reply, and the failure when the turn failed); not after a stop, nor a wake that did nothing;
- deleting a child (a notice to its parent, in the delete's own transaction).

**At startup** (`agents::startup_sweep`, after the clones a restart cut off are failed): every child still `starting` fails with "smelt restarted during the start", and every conversation with undelivered rows that isn't a `stopped` agent is woken. The pause after a Stop is in memory, so after a restart a conversation the user had stopped is woken too (SME-238 stores it).

## Deleting

`delete_conversation` deletes the whole subtree (`agents::delete_subtree`): transaction A locks the group root's row (the lock `start_agent` takes) and marks the subtree `deleting_at` and its agents `stopped`, so a `start_agent` after it is refused; every turn and browsing session in the subtree is stopped and its pods listed; transaction B locks the deleted conversation's parent (shared) and the subtree (for update) in ascending id, queues the notice to the parent, and deletes, the cascade removing the subtree; then each conversation's pods are torn down and its turn state and event channel forgotten. The sidebar's armed Delete says how many agents go with it ("Delete + 2 agents?").
