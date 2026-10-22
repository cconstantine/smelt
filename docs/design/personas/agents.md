# Child agents, messages and limits
Part of the [personas design](index.md) (SME-234).

A child agent is a conversation (the project's scope item 1): its own transcript, model, streaming, history, Stop, usage and sandbox. This page gives the schema, the tools, how a message or a report enters a conversation's turn loop, the statuses, what Stop, `stop_agent`, deletes and restarts do, and the limits.

## The schema

- **`conversations.parent_id`** `BIGINT NULL REFERENCES conversations ON DELETE CASCADE`, indexed. Null for the user's own conversations. A cascade, so the database can never hold a child whose parent is gone (see [Deleting](#deleting)).
- **`agents`**, one row per child conversation (SME-236):

| Column | What |
| -- | -- |
| `conversation_id` | PK, FK → `conversations` cascade |
| `root_id` | The group's root, the user's conversation at the top; FK → `conversations` cascade |
| `depth` | 1 for the root's children, parent's + 1 below |
| `brief` | The brief, as `start_agent` got it (also the child's first message) |
| `persona_name`, `persona_path`, `persona_commit`, `persona_body`, `persona_settings` (JSONB) | The persona snapshot, all null without one ([persona-files.md](persona-files.md#read-from-the-childs-own-checkout)) |
| `rule_chain` (JSONB), `credentials` (`TEXT[]`) | The effective rules and credentials ([tool-rules.md](tool-rules.md#narrowing-a-child-never-gets-more-than-its-parent)), fixed at start |
| `state` | `active`, `finished` or `stopped`, `CHECK`ed; the only stored part of the status ([Statuses](#statuses)) |
| `created_at`, `state_changed_at` | |

- **`agent_messages`**, the queue (SME-236 for reports, SME-238 for the rest): `id` (identity), `to_conversation_id` (FK cascade), `from_conversation_id` (FK `SET NULL`, so a deleted sender's message still arrives), `from_label` (`agent #12 (smelt:developer)` or `your parent`, kept for that case), `kind` (`message`, `report`, `notice`), `body`, `created_at`, `delivered_at`, `delivered_message_id` (FK → `messages` `SET NULL`). A partial index on `(to_conversation_id, id) WHERE delivered_at IS NULL`.
- **`pending_questions.asked_of`** `TEXT NOT NULL DEFAULT 'user'`, `CHECK`ed `user`/`parent` (SME-238): a child's question to its parent uses `ask_user`'s own row and answer path (SME-34).
- **`conversations.paused_at`** `TIMESTAMP NULL` (SME-238): the pause after a Stop, stored (see [Restarts](#restarts)).

The `agents` row is separate from `conversations` so the user's own conversations carry none of it; `parent_id` is on `conversations` because the cascade has to be.

## Tools

| Tool | Who is offered it | What it does |
| -- | -- | -- |
| `start_agent(brief, repo?, ref?, persona?, volumes?)` | any conversation, within the limits | Creates the child, records its parent, saves the brief as its first message and starts its first turn in the background once its repo is cloned and its persona read; returns the child's id at once. `persona` requires `repo`; `ref` is [checkouts-and-volumes.md](checkouts-and-volumes.md#which-ref-the-childs-clone-is-on)'s; `volumes` is SME-239's. |
| `list_agents()` | any conversation | This conversation's own children: id, title, persona (`<dir>:<name>` or none), status, `created_at`, the time of its last message, and how many agents it has started. Never anyone else's children. |
| `stop_agent(id)` | any conversation | Stops one of its own children and everything under it ([Stop](#stop-stop_agent-and-the-user)). Refuses any other id. |
| `message_agent(to, text)` | a conversation with a parent or children | `to` is `parent` or one of its own children's ids. Queues the message and wakes the recipient. Refuses anyone else (siblings go through the parent). |
| `ask_parent(question)` | a child | Records a question to its parent and ends its turn; the parent's next `message_agent` to it answers it and wakes it. |
| `finish(report)` | a child | Its final report: queued to the parent, the child's state becomes `finished`, and its turn ends. |

A conversation without a parent isn't offered `ask_parent` or `finish`. Until SME-241's rules, only a conversation without a parent is offered `start_agent` (so SME-236 ships with depth 1 and no persona field for it); from SME-241 on, `start_agent` is a tool like any other under a persona's rules, plus the depth limit. There's no separate `may_start_agents` field: one mechanism, not two.

## How a child's report and messages reach a conversation

**Decision: a durable queue, `agent_messages`, drained under the recipient's turn lock like finished commands, and woken with `Notice::Wake`.** Not `Notice::Deliver`.

What happens today (`src/turn/notify.rs`): everything outside a turn reaches the model through `turn::notify`, which spawns a task per call. `Deliver(text)` holds the text in that task's memory until it can take the turn lock; `Wake` holds nothing and drains what the database says is pending (`drain_unnotified_terminal_commands`, `drain_idle_command_notices`, run at the top of every turn-loop iteration in `run_turn_steps`), each saved with its mark in one transaction (SME-91).

| | `Notice::Deliver(text)` | Queue + `Notice::Wake` (chosen) |
| -- | -- | -- |
| smelt restarts before it lands | lost: the spawned task and its text are gone | kept: the row waits, and the startup sweep wakes it |
| Several children report at once | one turn per report, in whatever order the tasks take the lock | one wake drains them all, in `id` order, into one model call (as SME-13 does for commands) |
| Recipient busy | waits for the lock, then its own turn | the running turn takes it at its next iteration, between a tool result and the next call, never inside a call |
| Recipient paused by Stop | saved, no turn | stays queued, no wake; drained by the user's next message, or by a wake after the pause ends |
| Recipient deleted | the task fails | the row cascades away; `message_agent` refuses a deleted recipient |
| A record of who sent what | none | the row, linked to the message it became |

The drain, `drain_agent_messages`, runs beside the two command drains in `run_turn_steps`, and `db::wake_has_work` gains "an undelivered `agent_messages` row for this conversation". Each row becomes one `user` message whose first line names the sender (`Message from agent #12 (smelt:developer):`, `Report from …`, `Notice: …`), saved and marked delivered in one transaction. The transcript shows it as from that agent through `delivered_message_id`, a stored link, not by parsing the text.

**A child's final report** is its `finish(report)` call, not "its turn ended": a Developer ends many turns waiting on its gate, and each would otherwise reach the parent as a report. `finish` inserts the report and sets `finished` in one transaction, then the turn loop ends the turn the way `ask_user` does (`run_tools`' `asked`).

**A child that stops without finishing** would leave its parent waiting for good. So when a child's last turn ends and the child is `active`, with no question waiting, no terminal command running, no `active` agent of its own (a parent waiting on its agents ends its turn too) and no Stop, smelt queues a notice to the parent: `agent #12's turn ended without a report; its last message: <first 500 characters>`. A command finishing just after the check wakes the child again, which is fine: the notice says the turn ended, not that the work did.

**Asking and waiting:** `ask_parent(question)` records a `pending_questions` row with `asked_of = 'parent'` and queues the question to the parent as a `message`; the child's turn ends waiting, as with `ask_user`. The parent's next `message_agent(to: <child>)` is saved as the answer on that row (`answer`, the card's path) and wakes the child, whose turn takes it as the call's result (`take_answer_or_wait`). The user writing in the child instead takes the question with `NOT_ANSWERED`, as today; a later reply from the parent then arrives as a plain message.

## Statuses

Stored: only `agents.state`, the lifecycle (`active`, `finished`, `stopped`). Everything else is derived from where it already lives, so a status can't disagree with its source:

| Status (first that holds) | From |
| -- | -- |
| stopped | `state = 'stopped'` |
| running | a turn in flight (`turn::turn_running`, in memory) |
| waiting for the user | an unanswered `pending_questions` row with `asked_of = 'user'`, or an `instruction_requests` card |
| waiting for its parent | an unanswered `pending_questions` row with `asked_of = 'parent'` |
| finished | `state = 'finished'` |
| idle | otherwise (between turns, maybe waiting on a command) |

**Every writer of `state`:** `finish` (`active` → `finished`); `stop_agent` (the subtree → `stopped`); the drain delivering a parent's message to a `finished` child (→ `active`, in the drain's transaction); the user's own send or answer in the child (`finished` or `stopped` → `active`, in `start_turn`'s admission). Each is one `UPDATE … WHERE state = <expected>`, so two writers can't both act on one state: a `finish` racing a `stop_agent` leaves whichever committed second refused, and `stop_agent` wins over a later `finish` because `finish` checks `state = 'active'`.

## Stop, `stop_agent` and the user

- **The user's Stop on a conversation stops that conversation only,** its parent and children untouched. It pauses it (stored, below); messages to it queue without waking it. Its parent gets a notice, `the user stopped agent #12`, so the parent doesn't wait for a report that won't come. Stopping a whole group is the budget's stop (SME-245) or `stop_agent` from the top; a "stop this group" button is SME-234's question 2.
- **`stop_agent(id)`** sets the child and every descendant to `stopped` (one `UPDATE` over the subtree, a recursive CTE on `parent_id`), then calls `turn::stop_conversation` on each, as `outside_turn.rs` asks of "server code that stops another conversation (SME-236's `stop_agent`)". Pods and terminals are left alone, as Stop leaves them, so the user can look. Messages to a stopped agent are refused; only the user writing in it makes it `active` again. A stopped child's descendants are stopped too: their work was for it.
- **The user writing in a child** is an ordinary turn: it ends the pause and makes a `finished` or `stopped` child `active`. Its replies don't go to the parent; the child reports when it calls `finish` again.

## Deleting

**Deleting a conversation deletes its whole subtree** (SME-234, question 3): `conversations.parent_id`'s cascade removes the rows, and `delete_and_forget` (`src/api/chat.rs`) first collects the subtree's ids and pods, stops each conversation's turns and tears each one's pods down, as it does for one today; a pod started meanwhile is found by `sweep_orphaned_conversation_claims`. Volumes the subtree created go with it ([checkouts-and-volumes.md](checkouts-and-volumes.md#deleted-with-the-conversation-that-created-it)). The delete asks first, naming how many agents it takes with it.

**Deleting a child** queues a notice to its parent (`the user deleted agent #12`), in the delete's own transaction, so it lands only if the delete does. The parent's `list_agents` no longer shows it; messages it had queued to the parent still arrive (`from_conversation_id` `SET NULL`, `from_label` kept).

## Restarts

Turn state is in memory (`src/turn/state.rs`), and a restart loses every turn in flight and every pause. With agents waking each other, both matter:

- **The pause is stored** (`conversations.paused_at`), set by Stop and cleared by the user's send or answer, where `paused` is set and cleared today. The in-memory flag was harmless while nothing woke a conversation on its own after a restart; the startup sweep below does.
- **A startup sweep** wakes (`Notice::Wake`) every conversation with an undelivered `agent_messages` row that isn't paused and isn't `stopped`.
- **A child cut off mid-turn** (`active`, its last message a `user` message: a tool result or a notice with no reply after it) gets a notice of its own, `smelt restarted during your turn; carry on`, and a wake, so a group doesn't stall on a restart. A user's own conversation is left as today.

## State × event

For a conversation in a group. "Queued" is an `agent_messages` row; "wake" is `Notice::Wake`. SME-238's plan owns the full model and its tests; this is the table it starts from.

| State ↓ / event → | Message from parent or child | Child's report or notice (this is the parent) | User's Stop on this | User's Stop on its parent | `stop_agent` on this | User writes in this | smelt restarts | Its parent deleted | This deleted | Waiting for a provider slot |
| -- | -- | -- | -- | -- | -- | -- | -- | -- | -- | -- |
| idle | queued, wakes, runs a turn | queued, wakes | paused; parent told | nothing | stopped, subtree too | turn runs | sweep wakes it if queued | deleted with it | parent told | — |
| running | taken at the next iteration | taken at the next iteration | turn ends, paused; parent told | nothing | turn ends, stopped | queued behind the turn (turn lock) | turn lost; restart notice and wake | deleted, turn ended | turn ended, rows gone | the call waits; status running |
| paused (Stop) | queued, no wake | queued, no wake | stays paused | nothing | stopped | pause ends, turn runs and drains | stays paused (stored) | deleted with it | parent told | — |
| waiting for the user | queued; the turn it wakes saves it and waits on | as left | paused | nothing | stopped | the user's message takes the question | unchanged | deleted with it | parent told | — |
| waiting for its parent | the parent's message answers the question and wakes it | as left | paused | nothing | stopped | takes the question (`NOT_ANSWERED`) | unchanged | deleted with it | parent told | — |
| finished | `active`, wakes | queued, wakes | paused | nothing | stopped | `active`, turn runs | unchanged | deleted with it | parent told | — |
| stopped | refused at `message_agent` | queued, no wake | stays | nothing | already | `active`, turn runs | unchanged | deleted with it | parent told | — |

**Check-then-act, each named for SME-238's plan** (SME-34's lesson: the state can change between a check and the act): `message_agent`'s "is this my child, not stopped" check and its insert go in one transaction that locks the recipient's `agents` row (`FOR SHARE`), against `stop_agent`'s `UPDATE`; the drain marks a row delivered in the transaction that saves the message, so a Stop between them can't save it twice (SME-91); `stop_agent` updates the state before stopping turns, so a wake admitted between them is ended by the stop (SME-217's order); the "turn ended without a report" check reads state, question, commands and pause in one query after the last turn's `TurnInFlight` is dropped.

## Limits (SME-245)

| Limit | Counted | Checked | Over it |
| -- | -- | -- | -- |
| Agents at once | The group's `active` agents (`agents.root_id = root AND state = 'active'`): live work, not model calls | In `start_agent`'s transaction, which locks the root's `conversations` row (`FOR UPDATE`) before counting, so two starts can't both pass | Refused with a tool error naming the limit; no waiting inside a tool call |
| Nesting depth | `agents.depth` | When the tool list is built (not offered at the limit) and in `start_agent` | Not offered; a call is refused |
| Budget | The group's spend: `model_call_usage.cost_usd` summed by a new `model_call_usage.group_root_id` (no FK, so a deleted child's spend still counts; `conversation_id` is `SET NULL` on delete) | Before each model call of a group member, under its turn lock | No call; every member's turn is stopped and paused, the root marked `budget_stopped_at`, each told why; the user lifts it on the root |

**Defaults** when no persona sets them, which also govern the usual group (its root is the user's own conversation, which has no persona): agents at once 4, depth 2, no budget, as deployment settings (`SMELT_AGENTS_AT_ONCE`, `SMELT_AGENT_DEPTH`, `SMELT_GROUP_BUDGET_USD`). A persona's `smelt.limits` apply to the subtree under it and combine with its ancestors' by taking the smaller. The budget can overshoot by the cost of the calls already in flight when it's reached (each member's check passed before any of them recorded): at most one call per running member.

**The provider's own limit (SME-162)** is a slot per model call (`src/providers/limiter.rs`: held "from before it sends until it ends"). A parent waiting on its children holds no slot: `start_agent` returns at once, and a parent waits by ending its turn, not inside a call. So a child queued on a slot can't deadlock a parent waiting on it, and "agents at once" counts live agents, not slots. A child queued on a slot shows as running.

## SME-38

SME-38 ("a real subagent dispatch tool") is a duplicate of SME-236 (project question 1). Its open questions are answered here: a child has its own conversation row and transcript (this page), its own sandbox ([checkouts-and-volumes.md](checkouts-and-volumes.md)), and may start its own children within the depth limit.

**Alternatives considered on this page:** opencode's `subagent` tool (`packages/core/src/tool/plugin/subagent.ts`) runs a child session in the foreground or background, notifies the parent on completion, and has a depth limit (`experimental.subagent_depth`, default 1); Codex's multi-agent tools (`spawn_agent`, `followup_task`, `send_message`, `codex-rs/prompts/src/model_messages/multi_agent.rs`) deliver messages tagged `MESSAGE` or `FINAL_ANSWER` and treat a child's final-channel output as its report; Claude Code's subagents (code.claude.com/docs/en/sub-agents) default to three layers deep and 20 running at once. smelt's child is a whole conversation the user can open and steer, which none of these is, so it can't block its parent on a foreground call: every child is "background", and its report is an explicit call, as Codex's final answer is an explicit channel.
