# Pod management, and stopping a turn

**Branch:** `pod-management` · **Idea:** `projects/ideas/pod-management.md`

## What

Two things the user can't do today:

1. **See and stop sandbox pods.** Only the model creates and ends pods. A pod runs until the model terminates it or its conversation is deleted, so pods in old conversations hold cluster memory and CPU with no sign they exist. The fix:
   - a marker in the sidebar on conversations with a live pod;
   - a pods view listing every live pod, with its conversation, status, uptime, idle time, limits, live memory and CPU use, and terminal count;
   - a Stop button, both in the pods view and in the conversation's sandbox panel.
2. **Stop the model's turn without stopping its pod.** When the model is going the wrong way or is stuck in a loop, the user can only wait. The fix is a Stop button that ends the current turn at once, whether the user started it with a message or a finished command or background task woke the model. The pod, its terminals and any running commands are left alone.

Decided at planning (2026-09-25):
- Stop only, no restart: the model creates a fresh pod when it needs one.
- Stopping a pod mid-turn is allowed.
- The pods view shows idle time, but there's no automatic idle cleanup (that stays in `coding-session.md`).
- Live memory and CPU use is shown, not just limits.
- The user doesn't create pods.

## How

### Part 1: seeing and stopping pods

**Data**
- `db::list_live_pods(pool)`: every pod with `terminated_at IS NULL` (already indexed), joined with its conversation's title and `updated_at`, and with each pod's live terminal count and last command activity (`MAX(finished_at)`, or "running now" if a command has no `finished_at`).
- `get_conversations` gains `has_live_pod: bool` (an `EXISTS` subquery), for the sidebar marker.

**Kubernetes**
- Status and limits come from the pod object smelt already reads (`status.phase`, the container's `resources.limits`).
- **Live use** comes from the metrics API (`/apis/metrics.k8s.io/v1beta1/namespaces/<ns>/pods`), read in one list call per refresh with `kube::Client::request`, since `k8s-openapi` has no metrics types.
  - smelt's service account currently gets **403** there. `k8s/smelt-park-rbac.yaml` gains `get`/`list` on `pods` in the `metrics.k8s.io` group, for both namespaces.
  - The local k3s picks this up when `scripts/k3s-bootstrap.sh` runs (on `docker compose up`). **Homelab needs you to apply it.**
  - Until then, or if the metrics call fails for any reason, the usage column shows "unavailable" and everything else works.
  - Metrics lag by up to about a minute and are missing for a pod that just started; the view says "no data yet" in that case.

**Idle time**
- A pod is **busy** while any of its terminals has a running command.
- Otherwise it's **idle since** the latest of: its last command finishing, its conversation's last message, and the pod starting.
- A message also stands in for file and tool activity, which leaves no per-pod record. Good enough to spot forgotten pods; not precise to the second.

**Server functions** (`src/api/pods.rs`, new)
- `get_pods() -> Vec<PodOverview>`, where `PodOverview` has:
  - `pod_id`, `conversation_id` and `conversation_title`;
  - `status`, `started_at` and `idle`, which is either busy or idle since a given time;
  - `memory_limit` and `cpu_limit`;
  - `usage: Option<PodUsage { memory_bytes, cpu_millicores }>`;
  - `terminals`.
- `stop_pod(pod_id)`.

**Stopping** (`sandbox::stop_pod_for_user`)
- Reuses the crash path's teardown: running commands are marked lost, terminals are closed with UI events, and the pod is force-terminated, whether or not terminals are open.
- The model is told in one message: "The user stopped sandbox pod N. Its terminals and any files outside mounted volumes are gone; create a new pod if you need one."
- **That message is saved under the conversation's turn lock**, in a spawned task, so it can never land between a tool call and its result. The API rejects that order, and the crash path has the same risk today, so both go through this one function.
- Stopping mid-turn is allowed. The model's in-flight tool calls just fail, as they would on a crash, and the notice is saved once that turn ends.

**Live updates**
- A new **app-wide event stream**, `subscribe_app_events()`, alongside the per-conversation one. It carries a single `PodsChanged` event, published whenever a pod is created, stopped, crashes or is torn down with its conversation.
- The sidebar and the pods view refetch on it. Sandbox events are per-conversation today, so without this a pod created in another tab, or by a background notification, wouldn't show up until a reload.
- Built with `ServerEvents::from_stream`, like the other streams, so a closed tab drops its subscription.
- The pods view also refetches every 30s while open, to keep uptime, idle time and usage current.

**UI**
- **Sidebar:** a small marker on each conversation with a live pod.
- **`/pods`:** a table with one row per live pod. The conversation links to it, and each row has a Stop button with two-click confirmation like conversation delete. A link to it goes in the sidebar, next to the MCP servers and volumes pages.
- **The sandbox panel:** gains the same Stop button for the conversation's own pod.

### Part 2: stopping a turn

**Cancelling**
- `run_turn_bounded` registers a per-conversation cancel signal for the length of the turn. It runs its body in a `select!` against that signal, so a stop drops the body at whatever it's awaiting: the model's stream, a tool call, or a compaction.
- Dropping the body releases the turn lock as usual.
- `stop_turn(conversation_id)` fires the signal, and it's a no-op when no turn is running.
- This covers every kind of turn: a user's message (`send_message`), a finished terminal command (`wake_conversation`), and a background task's notification.

**What's left behind**
- A reply that was streaming isn't saved.
- Tool calls the model had made but that hadn't finished are left without results.
- **A new repair step at the start of every turn** finds tool calls with no result after them and saves an error result for each ("stopped by the user before this finished"). The history is then valid again.
- The same step also repairs a turn cut off by a server restart, which can leave the same state today.

**What isn't touched**
- The pod, its terminals, and a command already running in one keep going. Stopping a turn doesn't kill commands; `send_signal`, or stopping the pod, does.

**Not waking back up**
- A command or background task finishing after a stop would normally wake the model and start a new turn, which isn't what "stop" means.
- So a stop **pauses** the conversation: completion notices are still saved, but they don't start a turn until the user sends the next message. That message clears the pause, and the model then sees both the notices and the message.

**Showing it**
- A new per-conversation event, `TurnState { running: bool }`, is published when a turn starts and ends. A `get_turn_state` snapshot covers reconnects.
- While a turn runs, the composer shows **Stop** next to the input. That includes turns the user didn't start from this tab.
- After a stop, the chat shows a small "Stopped" note where the reply was.

## Tests (test-first)

**Part 1**
- `list_live_pods` returns only live pods, with the right title, terminal count and last activity; `has_live_pod` is right in both directions.
- The idle calculation: busy while a command runs, and otherwise the latest of the three times. It's a pure function over the rows.
- Parsing a metrics response into `PodUsage`, from a real captured response. That response is also the fixture for the "real artifacts" rule, captured once RBAC allows it.
- `get_pods` with metrics returning 403 still returns every pod, with usage `None`.
- `stop_pod_for_user`:
  - marks running commands lost and closes terminals, even with terminals open;
  - saves the notice once, and only after an in-progress turn releases its lock (a test holds the lock and checks nothing is saved until it's released);
  - publishes `PodsChanged`.
- The crash path now also waits for the lock before saving its message.
- `subscribe_app_events` delivers `PodsChanged`, and a dropped subscription stops listening (like the existing test for conversation events).
- A real-cluster test: stop a pod that has an open terminal and a running command.
- Browser tier: the sidebar marker appears and disappears live as a pod is created and stopped in another conversation, and Stop in `/pods` removes the row.

**Part 2**
- A turn whose fake model stream never finishes stops within a second of `stop_turn`, and the turn lock is free afterwards.
- A turn stopped mid-tool-call leaves an unanswered tool call, and the next turn's repair step saves an error result, so the request it then sends is valid.
- `stop_turn` with no turn running does nothing.
- After a stop, a finished command's notice is saved but doesn't start a turn. The next user message clears the pause, and the turn includes both.
- `TurnState` is published at a turn's start and end, including for a turn a notification started.
- Browser tier: Stop appears while a turn runs (a slow fake model), ends it, and the input is usable again.

## Which files

- `migrations/`: none expected. The pause flag lives in memory with the other per-conversation turn state; a restart clears it, which is harmless.
- `src/db.rs`: `list_live_pods`, `has_live_pod` in the conversation list query, and the unanswered-tool-call lookup.
- `src/sandbox.rs`: `stop_pod_for_user`; limits and phase for the overview; the metrics read; the crash message moved behind the lock; publishing `PodsChanged`.
- `src/events.rs`: an app-wide bus (`AppEvent::PodsChanged`) next to the per-conversation one, plus `ConversationEvent::TurnState`.
- `src/api/pods.rs` (new): `get_pods`, `stop_pod`, `subscribe_app_events`.
- `src/api/chat.rs`: the cancel signal and `select!` in `run_turn_bounded`, `stop_turn`, `get_turn_state`, the repair step, the pause, and `TurnState` events.
- `src/anthropic/tools.rs` and the terminal notification path: respect the pause.
- `src/frontend/pages/pods.rs` (new) and `src/frontend/mod.rs` (the `/pods` route); `src/frontend/pages/chat.rs`: the sidebar marker, the app-events subscription, and the Stop buttons (pod and turn); `assets/chat.css`.
- `k8s/smelt-park-rbac.yaml`: metrics read access.
- `src/browser_tests.rs`: the scenarios above.
- Docs:
  - `api.md` (new endpoints and events);
  - `frontend.md`;
  - `setup.md` (applying the RBAC change);
  - `testing.md`;
  - close-out as usual, including `coding-session.md`'s idle-pods item, reworded to "automatic cleanup" now that idle time is visible.

## Decisions from review (2026-09-25)

1. **One PR** for both parts, with commits kept separate per behavior.
2. **Pause after a stop:** yes. Notices are saved, and don't wake the model until the user's next message.
3. **Homelab RBAC:** fine to ship with usage "unavailable" there; no homelab work in this project.
4. **Idle definition:** as proposed. Busy while a command runs; otherwise idle since the last command, message or pod start.
5. **Sidebar marker:** a plain dot with the tooltip "sandbox pod running", with no busy/idle distinction.
