# Pod management, and stopping a turn

**Branch:** `pod-management` · **Idea:** `projects/ideas/pod-management.md` (removed) · **Plan:** `projects/plans/pod-management.md` (removed)

## What shipped

### Seeing and stopping pods

- **Sidebar:** a green dot on each conversation with a live sandbox pod, updated live.
- **`/pods`** (linked under "MCP servers") lists every live pod:
  - its conversation and status;
  - uptime, and activity: busy while a command runs, otherwise idle since the latest of its last command, its conversation's last message, and the pod starting;
  - memory and CPU use against the configured limits;
  - terminal count;
  - a two-click Stop.

  It refreshes when a pod comes or goes, and every 30s.
- **The sandbox panel** has the same Stop.
- **Stopping** tears the pod down whether or not terminals are open, the way a crash does: running commands are marked lost and terminals closed. The model gets one notice ("The user stopped sandbox pod N…") and isn't woken; it learns on its next turn.
- **Notices wait for the turn to end.** The notice is saved only between turns (`save_notice_between_turns`, under the turn lock), so it can't land between a tool call and its result, which the API rejects. The pod-crash path had that risk too, and now goes through the same function.
- **Live usage** comes from the cluster's metrics API. That needs a new RBAC rule (`k8s/smelt-park-rbac.yaml`); without it, or on any metrics failure, usage shows "unavailable" and the rest works.

### Stopping a turn

- **The button:** **Stop** appears next to Send while a turn runs, in every tab watching the conversation, whoever started the turn (`ConversationEvent::TurnState`, plus a `get_turn_state` snapshot on reconnect).
- **What a stop does:** it ends the running turn and any queued behind it, and leaves the pod, its terminals and running commands alone. A reply that was streaming isn't saved.
- **Unanswered tool calls:** tool calls without a result get an error result when each request is built (`answer_unfinished_tool_calls`), so the history stays valid. This also fixes the same broken state that a server restart mid-turn could leave before.
- **The pause:** a stop pauses the conversation. Finished commands and background tasks still save their notices, but don't start a turn until the user writes again.

### Changes from the plan

- **Tool-call repair happens at request time, not saved at the next turn's start.** Messages are ordered by creation time, and notices can already follow a dangling tool call, so a saved result would land too late. Building the history can put the result right after the call.
- **Chat tabs don't get a second always-open stream.** The plan had the sidebar subscribe to a new app-wide stream. Doing that made the browser tier's seventh tab never finish loading. Chrome allows 6 HTTP/1.1 connections per host, shared by every tab, and each chat tab already held one. So `PodsChanged` is relayed on each tab's existing conversation stream instead, and only `/pods` uses the app-wide stream. The cost: on `/`, with no conversation open, the sidebar dots only refresh on navigation.
- **The sidebar gets its list from `get_live_pod_conversations`,** not a `has_live_pod` field on `Conversation`. Adding a field would have broken every query that loads a conversation.
- **`PodOverview` carries `observed_at`,** the database's `now()`, so ages are measured on one clock rather than the browser's.

### Tests

Each seen failing first:
- **Database:** the live-pods query, including only live pods and ignoring closed terminals.
- **Pure functions:** the idle calculation, and Kubernetes quantity and metrics parsing (against the documented response shape; see "Not done").
- **Real cluster:** a user stopping a pod with an open terminal and a running command. The command is marked lost, the terminal and pod are gone, the notice arrives, and it's not reported as a crash. `PodsChanged` is published on create and stop, and the pods view reports status, limits, busy and terminals correctly.
- **Notices:** a notice waits for a held turn lock.
- **Event streams:** the app-wide stream drops its subscription with the tab, and the conversation stream relays `PodsChanged`.
- **Stopping:**
  - a turn stuck waiting on the model ends within a second of Stop, frees the lock and keeps the user's message, and a later turn isn't affected;
  - unanswered tool calls are answered in all three positions;
  - `TurnState` is published at start and end, including for a stopped turn;
  - a stopped conversation doesn't wake for a finished command until the user writes, and that turn includes the notice;
  - a paused conversation saves a task's notice without starting a turn;
  - deleting a conversation drops its stop registration.
- **Browser tier:**
  - scenario 12: a pod starting in one conversation puts a dot in another tab's sidebar without a reload, `/pods` lists it with its conversation, Stop removes the row, and the dot goes away;
  - scenario 13: Stop shows in the sending tab and a watching tab, stopping says "Stopped.", Stop goes away in both, the rest of the reply never arrives, and the input is usable.
  - Each was seen failing on a deliberately broken build.

### Fixed after review

You found two problems after the PR was opened (first written up as ideas `stale-live-pod-dots` and `stable-stop-button`, since removed).
- **Dots for pods that no longer existed.** The database listed 29 live pods; the cluster had one. The other 28 were from 22–24 September: pods lost in the cluster rebuild and namespace cleanups, and pods the browser tier had created in the *test* namespace against the shared dev database. Nothing ever closed a record whose pod vanished while smelt wasn't connected to it (crash detection only works for a pod with a live connection). Some even showed as busy, because their commands were still marked running.
  - `sandbox::watch_pods`, started from `main()`, now keeps records in step with the cluster, in this order: it subscribes to pod changes (a Kubernetes watch), reconciles once the first full listing arrives (closing live records older than 5 minutes whose pod isn't listed), then closes a record when its pod is deleted or finishes, after a 30-second grace period. The watcher re-lists after any reconnect, so nothing missed while it was down stays stale. Pods smelt is connected to are left to crash detection, which also tells the model. A first version polled every minute; you asked for the watch instead.
  - It runs from `main()` only, never the browser harness, which works in the test namespace and would close the dev instance's records.
  - Running commands are marked lost and terminals closed, like a crash, but quietly: a notice per conversation would move every old one to the top of the sidebar, and the model finds out if it tries the pod again.
  - Rows younger than 5 minutes are skipped, since a row exists briefly before its pod does.
  - Covered by a real-cluster scenario, seen failing on a stub. A pod gone before the watch starts is closed by its first listing, with its command lost and terminal closed. A pod deleted while the watch runs is closed after the grace period. No notice is saved, the conversation doesn't move, and a young record is left alone. The `Failed`/`Succeeded` path isn't exercised directly.
- **A Stop button that grew when armed.** "Stop" became "Confirm stop?" and the button widened (43 to 99 pixels in the browser tier), so the confirming click could miss.
  - All five two-step buttons now use `TwoStepLabel`: both labels share one grid cell with the inactive one hidden, so the button is always as wide as its longer label. The five are pods Stop, sandbox-panel Stop pod, conversation delete, volume delete, and MCP server delete.
  - Scenario 12 now measures the button and its neighbouring cell before and after arming. It was seen failing on the old buttons.

### Not done

- **Apply the RBAC change to the local k3s:** run `docker compose up` (or `docker compose run --rm k3s-bootstrap`) on the host. The `docker` CLI in the dev container talks to a sidecar, not your compose stack, so I couldn't. Until then, usage shows "unavailable" locally. Homelab isn't in scope, per the review.
- **The metrics fixture is the documented response shape, not a captured one.** It should be replaced by a real capture once the RBAC change is applied (the "fixture real artifacts" rule).
- **The HTTP/1.1 connection limit is a real ceiling for the user too.** A chat tab holds one always-open stream, and two while a reply streams. With about four smelt tabs open while a reply streams, a new tab can hang, or a Stop click can wait until the reply finishes. Serving over HTTP/2 (which browsers only use over TLS), or carrying replies on the conversation stream instead of a separate one, would lift it. Not in this project; worth an idea if it bites.
- **No live dot updates on `/`** with no conversation open (see "Changes from the plan").
- **Automatic idle cleanup** stays in `coding-session.md`, now reworded as "stopping idle pods automatically".

## Retrospective

**What worked:**
- **The browser tier caught a design problem no unit test could.** The per-tab app stream passed every unit test, and was only exposed when the seventh tab hung. Checking the theory by removing the stream (rather than guessing) confirmed it in one run, and pointed to relaying on the existing stream.
- **Watching every check fail first, including by deliberately breaking the build** (a stop that does nothing, a missing event handler, an overwriting insert, a notice saved without the lock). Several checks would otherwise have passed vacuously against stubs.
- **The plan's review answers** (one PR, pause after stop, limits of the homelab work) settled everything up front; nothing had to be re-asked mid-implementation.

**What caused friction, surprise, or rework:**
- **Process-wide state keyed by per-test ids.** The new pause and stop registries are keyed by conversation id, and every `#[sqlx::test]` database numbers conversations from 1. So one test's stop paused another test's conversation, and five unrelated tests failed, only in a full run. Fixed with unique ids (`create_conversation_with_id`) and written up in testing.md.
- **The connection limit came up twice.** First with the per-tab app stream (a new tab hung), then with Stop itself (the click queued behind a streaming reply while older tabs held connections). The second only showed as a timing-dependent failure, and was confirmed by closing tabs. Both are written up in frontend.md and testing.md.
- **My edits kept rebuilding your dev server's bundle,** which the browser tier also serves. It helped once (a run I expected to be stale was current), but it means a "stale bundle" failure-first run can't be relied on while your `dx serve` is running. I switched to breaking the code on purpose instead.
- **I couldn't apply the RBAC change to the local cluster myself,** so live usage is verified only as "unavailable degrades cleanly", not with real numbers.

- **Both review findings slipped past tests that looked at the right things with the wrong data.** The sidebar tests created fresh pods in a clean test database, so a record outliving its pod never came up; the dev database had 28 of them. The pods-page scenario clicked Stop and Confirm by selector, which works whether or not the button moves. Checking the running dev app against its real data, and measuring layout rather than just finding elements, would have caught both before review.

**Process suggestion (not applied; needs agreement):**
- Add to development-process.md: before adding an always-open stream (or any long-lived request) per browser tab, count the tab's open connections against the 6-per-host HTTP/1.1 limit, and prefer relaying on an existing stream. It cost two debugging rounds here.

**Bug bash: due.** This is the third project since the 2026-09-24 bug bash (websearch, system-prompt, pod-management).
