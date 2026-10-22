# Live conversation events

Part of [api.md](../api.md). How the page hears about changes as they happen.

These server functions exist to support live-updating panels (the sandbox panel — see SME-10 — the context-usage indicator/detail view — see SME-18 — the todo panel via `todowrite`/`todoread` — see SME-20 — and the browsing-session panel — see SME-22) and turns pushed from outside a request. `list_conversation_repos`, `get_turn_state`, `get_reply_in_progress` and `get_turn_error` (see the endpoint table) are the same kind of one-shot snapshot:

```rust
#[get("/api/conversations/{id}/todos")]
pub async fn get_todos(id: i64) -> ServerFnResult<Vec<anthropic::tools::TodoItem>>;

#[get("/api/conversations/{id}/sandbox")]
pub async fn get_sandbox_state(id: i64) -> ServerFnResult<SandboxSnapshot>;

#[get("/api/conversations/{id}/context-usage")]
pub async fn get_context_usage(id: i64) -> ServerFnResult<ContextUsageSnapshot>;

#[get("/api/conversations/{id}/context-detail")]
pub async fn get_context_detail(id: i64) -> ServerFnResult<ContextDetailSnapshot>;

#[get("/api/conversations/{id}/browsing")] // in api::browsing
pub async fn get_browsing_state(id: i64) -> ServerFnResult<BrowsingState>;

#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(id: i64) -> ServerFnResult<ServerEvents<ConversationEvent>>;
```

`get_todos` is a one-shot snapshot for the todo panel: the conversation's current todo list, as last set by `todowrite` (`db::get_conversation_todos` — empty if never called). `get_sandbox_state` is the same shape for the sandbox panel: every pod and terminal currently live in the conversation, each terminal hydrated with its `HISTORY_LIMIT` most recent commands (oldest first), each with the last 200 lines per stream, merged back into one true chronological `output` (stdout and stderr are fetched/capped independently so one stream can't crowd the other out of the window, then re-sorted by `seq` — see `fetch_command_summary` — so the panel doesn't show "all stdout, then all stderr"). Older history beyond the limit isn't duplicated here, it's still reachable through the model's own `list_commands`/`read_terminal_output` tools:

```rust
pub struct SandboxOutputLine { stream: String, data: String, seq: i64 }
pub struct SandboxCommandSummary { command_id: String, command: String, status: String, exit_code: Option<i32>, output: Vec<SandboxOutputLine> }
pub struct SandboxTerminalSummary { terminal_id: i64, pod_id: i64, status: String, commands: Vec<SandboxCommandSummary> }
pub struct SandboxPodSummary { pod_id: i64, status: String, terminals: Vec<SandboxTerminalSummary>, previews: Vec<events::SandboxPreview> }
pub struct SandboxSnapshot { pods: Vec<SandboxPodSummary> }
```

`get_context_usage` is the always-visible indicator's one-shot pull — the last real `usage` numbers persisted for this conversation (`None` if no turn has completed yet) plus the configured model's context window. `get_context_detail` is the click-through view: the same usage numbers, plus the exact system prompt a turn sends (from the same `system_prompt`/`prompt_environment` pair), the loaded `AGENTS.md` files in it with where each came from, every available tool's full definition, and the current message count — reconstructed from current state each call, not a stored snapshot of some specific past request:

```rust
pub struct ContextUsageSnapshot { usage: Option<anthropic::TokenUsage>, context_window: u32 }
pub struct ContextDetailSnapshot { system: Option<String>, instructions: Vec<git::ProjectInstructions>, tools: Vec<anthropic::ToolDefinition>, message_count: usize, usage: Option<anthropic::TokenUsage>, context_window: u32 }
```

`get_browsing_state` (`api::browsing`) is the browsing panel's own one-shot check — is a session open right now, and on what URL (`browsing::current_url`) — so the panel can show an idle state instead of trying to subscribe to a frame stream that doesn't exist yet, and fill its address bar:

```rust
pub struct BrowsingState { session_open: bool, url: Option<String> }
```

`subscribe_conversation_events` is the conversation's `ServerEvents` stream. It isn't scoped to one request: a browser tab opens it once per viewed conversation and keeps it open for as long as that conversation is selected, forwarding whatever `events::subscribe(id)` yields. A conversation's channel exists only while something subscribes: the first subscriber makes it, the last one's `Subscription` removes it, and `publish` with nobody subscribed drops the event without making one. A conversation that doesn't exist is refused (checked again once subscribed), so a tab still reconnecting to a deleted one makes nothing (SME-91):

```rust
// src/events/mod.rs
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ConversationEvent {
    MessagesAppended { messages: Vec<Message> },
    SandboxPodUpdate { pod_id: i64, status: String, terminated: bool },
    SandboxPreviewUpdate { pod_id: i64, previews: Vec<SandboxPreview> },
    SandboxTerminalUpdate { pod_id: i64, terminal_id: i64, status: String, terminated: bool },
    SandboxCommandUpdate { terminal_id: i64, command_id: String, command: Option<String>, status: String, exit_code: Option<i32>, stream: Option<String>, latest_output: Option<String>, position: Option<i64> },
    NotificationDeliveryFailed { detail: String },
    ContextUsageUpdate { usage: TokenUsage, context_window: u32 },
    TodoListUpdate { items: Vec<anthropic::tools::TodoItem> },
    BrowsingSessionUpdate { open: bool },
    BrowsingUrlUpdate { url: String },
    ReposUpdate { repos: Vec<git::RepoSummary> },
    PodsChanged {},
    TurnsChanged {},
    ModelChanged {},
    ProvidersChanged {},
    TurnState { running: bool, elapsed: u64 },
    ReplyReset {},
    ReplyDelta { text: String, offset: usize },
    TurnError { message: String },
}
```

`MessagesAppended` is a struct variant, not `MessagesAppended(Vec<Message>)`: the enum is internally tagged, and serde can't write a tuple variant holding a list that way. It used to be one, and every send failed silently (`wire_tests::test_every_event_round_trips_through_json` now serializes one of each variant). The stream is built with `ServerEvents::from_stream` over the broadcast receiver, for the same reason as the frame stream below: a closed connection drops the subscription. A subscriber that falls so far behind that the channel drops events for it (`RecvError::Lagged`) has its stream ended, so the tab reconnects and pulls the current state rather than missing a saved message or a turn's end. `TurnError` and `NotificationDeliveryFailed` split failures by who started the turn: a turn the user started by sending reports as `TurnError`; one started with no request in flight (a finished terminal command's `Notice::Wake`, a `Notice::Deliver`) reports as `NotificationDeliveryFailed`. A stop is neither's failure and isn't reported this way.

`SandboxCommandUpdate` with `command: Some` is a command starting: `sandbox::run_command` publishes it before sending the command to the pod's agent, so a fast command's finish, which comes back on the pod's reader task, can't reach a tab first. With `command: None` it's an output line (`stream` and `latest_output` set), a finish (`finished` and the exit code), or `lost` (`run_command`'s send failed; no exit code). The page applies each to the command with that `command_id`, which needn't be the terminal's newest (SME-144).

`Sandbox*`/`ContextUsageUpdate`/`TodoListUpdate`/`BrowsingSessionUpdate`/`BrowsingUrlUpdate` are all ephemeral UI telemetry (never persisted as such, regenerable at any time from `get_sandbox_state`/`get_context_usage`/`get_todos`/`get_browsing_state` — `ContextUsageUpdate`'s own numbers *are* separately persisted, in `conversation_context_usage`, precisely so `get_context_usage` can regenerate it, and `TodoListUpdate`'s are likewise persisted in `conversation_todos`); `MessagesAppended` is a live-delivery notification for rows `run_turn_bounded` already persisted — whether that turn came from a live `send_message` or from a notice that woke the model. `SandboxPodUpdate`/`SandboxTerminalUpdate` fire on create/terminate (a `terminated: true` update means the frontend should *remove* that pod/terminal, not just relabel it); `SandboxCommandUpdate` is one variant for "started", "one output line", "finished" and "lost", with `command` only populated on the "started" event. It carries `position` for an output line (the agent's per-command `seq`), so a tab that reconnects skips lines its snapshot already has. `ContextUsageUpdate` publishes once per completed real turn, right after `run_turn_bounded` persists that turn's usage. `TodoListUpdate` publishes on every `todowrite` call and always carries the *complete* current list (never a partial diff — `todowrite` itself is a whole-list replace, no per-item ids), so the frontend just overwrites its signal wholesale rather than merging line by line. `BrowsingSessionUpdate` publishes from `open_browser_session`/`close_browser_session`, so the panel shows/hides reactively rather than only checking on conversation (re)select. `SandboxPreviewUpdate` publishes from `sandbox_preview_url` and carries the pod's whole preview list (port and link), which the panel takes as is; `get_sandbox_state` returns the same list per pod (SME-42). Since the underlying `broadcast` channel has no replay, the frontend does a one-shot reconciliation pull on connect/reconnect to cover anything published before it subscribed: `get_messages`, `get_sandbox_state`, `get_context_usage`, `get_todos`, `get_pending_question`, `list_conversation_repos`, `get_turn_state`, `get_reply_in_progress`, `get_turn_error` and `get_browsing_state` — see [architecture.md](../architecture.md).

`ReposUpdate` carries the conversation's whole repo list (SME-32): each repo's path, what's checked out, clone status, its `AGENTS.md` files, which of them are loaded, and any loads waiting on the user's trust (with the file shown on the card). It's published when a repo is added, starts or finishes cloning, when the model loads or asks to load an `AGENTS.md`, and when a trust decision is made. `list_conversation_repos` is its snapshot.

`ModelChanged` says the conversation's provider or model changed (the picker, or a turn taking the default); the picker refetches `get_conversation_model`. `PodsChanged`, `TurnsChanged` and `ProvidersChanged` are the app-wide `AppEvent`s (below), relayed on every conversation's stream: the server merges the app-wide channel into each subscription (`conversation_event_stream`). A chat tab therefore needs no second always-open connection for the sidebar's pod dots and busy marks: on each, the sidebar refetches `get_live_pod_conversations` or `get_busy_conversations`. Over plain HTTP/1.1 a browser allows only 6 connections per host, shared by every tab, and a chat tab holds one stream (`subscribe_conversation_events`), plus `subscribe_browser_frames` while a browsing session is open. `TurnState` is published when a conversation's first turn starts and when its last one ends, including turns nobody started from a tab (a finished command, a clone or other notice, another tab). It carries `elapsed`, the busy spell's age in whole seconds on the server's monotonic clock at publish time: the start is recorded when `turns_in_flight` goes 0 to 1 and freed when it returns to 0, so it's one spell however many turns queue inside it. `get_turn_state` is its snapshot for reconnects — `(running, elapsed_seconds)` — so a tab that loads or reloads mid-turn shows the turn's real working time instead of restarting its "Working…" timer at 0s (SME-160). The pair is a new response shape, and of the two channels only the event one survives such a change: a bundle built before it ignores an event field it doesn't know, while its `get_turn_state` pull still expects a bare `bool`, fails to parse the pair, and so restores nothing at all. A stale bundle that reconnects mid-spell therefore keeps whatever turn state it last saw, and anything published while its stream was down is missed until the next event of that kind arrives — a turn that starts and ends inside the gap is missed outright. A tab already open across the deploy, with its stream intact, is unaffected: the turn's start reaches it as an event, so its Stop button and Working… line appear as they always did, the timer counting from its own load — until it reloads, which its stale-bundle banner asks it to do (SME-43).

## Stopping a turn

`stop_turn(id)` ends the conversation's running turn, and any turn queued behind it:
- **How:** each conversation has a stop counter (`TurnStops`, a `watch` channel, in its `turn::state` entry). A turn is admitted (counted, with the counter's value noted) by whoever starts it, before that returns or spawns it (`TurnInFlight`, SME-216): `send_message` and `answer_question` admit theirs before they return, so a Stop sent after either returns always ends that turn; a wake or a delivered notice is let in or refused by the pause in the same step that counts it (SME-217). In the order of those steps, a Stop ends exactly the turns counted before it. A turn also joins the turn lock's queue in that step (`TurnPlace`, SME-202), so queued turns run, or are saved when a Stop catches them, in admission order. That's not always the order the requests arrived in: `send_message` and `answer_question` admit only after awaited database checks (`start_turn` checks the conversation exists first), so two in flight at once (from two tabs, say) are admitted in the order their checks finish. One tab's sends and answers, each returning before the next is made, are admitted in the order they were made. `run_turn_bounded` then waits on that place in a `select!` against the counter changing, saves the turn's own message holding the lock and outside any `select!` (so a stop can't land between the save and the turn knowing it's saved; SME-91), then runs the turn (`run_turn_body`) in a `select!` against the counter. A stop drops the turn wherever it's waiting (the model's stream, a tool call, a compaction), which releases the lock. Turns that start after the stop aren't affected.
- **Left alone:** the pod, its terminals and running commands.
- **A deleted conversation:** `stop_turn` on a conversation that's gone, or never existed, leaves no turn state (it still ends any turn in flight there): it stops, then checks the conversation exists and forgets its state if not (`turn::stop_conversation`, SME-219). The check runs in a task of its own, so a Stop whose client hangs up still finishes it. Server code that stops another conversation (SME-236's `stop_agent`) must call it too. A failed "Work on a repo" shows its error the same way (`turn::show_error_outside_turn`). The delete's own stop is forgotten after its commit, and that forget runs in a task of its own too (`api::chat::delete_and_forget`, SME-262), so a delete whose client hangs up at any point, while the DELETE waits or during the teardown after it, still deletes the conversation, frees its turn state and closes its event channel.
- **What's saved:** a `STOP_NOTICE` user message ("The user stopped this turn before it finished…"), once however many turns the stop ended (`record_stop`, which records the last stop it noted), so the model doesn't take the request as still waiting for an answer; the chat page shows it as "Stopped.". A queued turn's own message that wasn't saved yet (a finished command's notice, say) is saved anyway, without running a turn for it, the way a turn that ran saves it (`save_turn_message`): an answered question's result goes first in it, and the user's own message also dismisses a question still waiting (SME-170). A turn stopped while queued keeps its place in the lock's queue, so queued messages a Stop catches are saved in admission order (for one tab's sends, the order they were sent; SME-202). Each ended turn fails with `TURN_STOPPED`, which a user's turn reports as `TurnError` (the page shows "Stopped.", and it isn't kept for `get_turn_error`).
- **What's left behind:** a reply that was streaming isn't saved; the stop clears it (`ReplyReset`), also while another turn is still queued. Tool calls the model had made but that hadn't returned have no result. `answer_unfinished_tool_calls` fixes that when each request is built: every tool call without a result gets an error result (`UNFINISHED_TOOL_CALL`), at the start of the next user message, or in a new one at the end. It isn't saved, since notices can already follow the dangling call. This also repairs a turn cut off by a server restart.
- **The pause:** a stop also pauses the conversation (`ConversationRuntime::paused`, in memory). While paused, `Notice::Wake` does nothing (a finished command's notice stays pending, and the next turn drains it), and a `Notice::Deliver` is only saved, as a `Notice::Save` is, instead of starting a turn. `send_message` and `answer_question` end the pause, in their turn's admission (`TurnInFlight::start_by_user`); a wake or a delivery checks it in its own (`TurnInFlight::start_unless_paused`), so none is counted after a Stop until the user sends or answers. Paused or not, a wake with nothing pending (no ended command's notice, no due idle notice, no answered question) isn't counted at all: it asks the database first (`db::wake_has_work`), before that admission, so no tab hears a `TurnState` for it (SME-218). Without the pause, a command still running would restart the model seconds after the user stopped it.

Everything outside a turn reaches the model through one function, `turn::notify(pool, id, notices)` (SME-52). It runs its notices in order in a task of its own and returns at once, since each waits for a running turn and the turn lock isn't re-entrant (a caller inside a turn could otherwise deadlock). `Notice::Save(text)` saves a `user` notice only while no turn holds the conversation's lock, so it can't land between a tool call and its result (an order the API rejects): the user stopping a pod, and the pod-crash notice. `Notice::Deliver(text)` also runs a turn for it, unless paused: Docker restarting, a language server stopping, a trust decision. `Notice::Wake` drains finished commands' notices and runs a turn for them, if any: a command finishing, and after the pod-crash notice. A failed delivery or wake is published as `NotificationDeliveryFailed`.

## The model's questions (`ask_user`, SME-34)

The model can ask the user one to four questions (`ask_user`: each a question, a header up to 30 characters, and two to four options, single or multi-select, or none for free text). The turn loop handles the call itself (`turn::questions`), not the tool registry:
- **Asking:** the other tools in that reply run as usual and their results are saved; a second `ask_user` in the same reply, or input over the limits, is an error result. Then a `pending_questions` row records the question (`answer` null), `QuestionUpdate { question: Some(..) }` and `AppEvent::QuestionsChanged` go out, and the turn ends with the call unanswered.
- **Waiting:** a turn that finds a question waiting saves what it brought (a notice, a finished command) and returns before calling the model (the check is in `run_turn_body`, after the drain), so every wake path waits too.
- **Answering:** `answer_question` checks the answer against the question and writes it onto the row with `UPDATE … WHERE answer IS NULL` (one tab wins; another gets "This question was already answered."), ends a Stop's pause and starts a turn. Whichever turn next holds the conversation's turn lock takes the row and puts the call's result at the front of its message, in one transaction with saving that message (`db::create_message_taking_question`), so a notice's turn that gets there first carries the answer instead. The result is `The user answered:` and one line per question (`1. <header>: <labels>[; they also wrote: "<text>"]`).
- **Writing instead:** a message the user sends while a question waits takes it (`from_user` on `run_turn_bounded`, set only by `start_turn`): the result is "The user didn't answer these questions. Their message follows.", then their text. A message a Stop catches still queued behind another turn takes it too (SME-170).
- **History:** notices saved while waiting sit between the call and its result, which the API rejects; `answer_unfinished_tool_calls` moves a result found in a later user message of the same run to the front of the message right after the call (`move_late_results`). Storage keeps the order things happened in.

## App-wide events and pods

`subscribe_app_events` is the stream of `AppEvent` (`src/events/mod.rs`), for views that span conversations. Both events carry nothing; listeners refetch:
- **`PodsChanged`**, published when a pod is created (`create_pod`), goes away (`force_terminate_pod`: a model's terminate, a user's stop, a crash), or is torn down with its conversation (`delete_conversation`).
- **`TurnsChanged`**, published when a conversation goes from idle to busy or back (`TurnInFlight`). Listeners refetch `get_busy_conversations`: every conversation with a turn running or queued.
- **`QuestionsChanged`**, published when a conversation starts or stops waiting on an answer to the model's question. Listeners refetch `get_waiting_conversations`, for the sidebar's waiting mark.
- **`ProvidersChanged`**, published when a model provider, a model's settings or the default model is added, changed or removed (SME-72). The model picker refetches.

Only the `/pods` page subscribes to it directly; chat tabs get both relayed on their conversation stream (see above).

`get_pods` returns a `PodOverview` for every live pod:
- **From the database** (`db::list_live_pods`): its conversation, start time, and live terminal count.
- **Activity** (`pod_activity`): busy while any terminal has a running command; otherwise idle since the latest of the last command finishing, the conversation's last message, and the pod starting.
- **From Kubernetes:** the pod object's phase and limits (`sandbox::pod_details`), and live use from one metrics list (`sandbox::pod_metrics_list`, `metrics.k8s.io/v1beta1`).
  - Reading metrics needs `get`/`list` on `pods` in the `metrics.k8s.io` group (`k8s/smelt-park-rbac.yaml`). Without it, or if the metrics call fails for any other reason, every pod's `usage` is `None` and the rest of the view still works.
  - Quantities are parsed by `parse_cpu_nanocores`/`parse_memory_bytes`, and CPU is summed across containers in nanocores.
- **Language servers** (`language_servers`): the server pods next to it, each with its state, memory limit and use (SME-35).
- **Agent** (`agent`, `sandbox::agent_status`): the protocol version its sandbox agent said hello with, and whether that's current, needs a restart for new features (an older minor), or needs one before its terminals and files work (another major, or an agent from before versioning). `None` until smelt has connected to it. The model's `list_pods` gives the same text (SME-53).
- **`observed_at`:** the database's `now()`, so the page measures ages on the database's clock rather than the browser's.

**Records are kept in step with the cluster.** A pod can vanish while smelt isn't connected to it: a cluster rebuild, a pod deleted outside smelt, one that died while smelt was down. Crash detection only notices a pod with a live connection. So `main()` starts `sandbox::watch_pods`, a Kubernetes watch on smelt's namespace (`kube::runtime::watcher`), which works in this order:
- **Subscribe first, then list.** When the watch's full listing arrives (`Init`, `InitApply`…, `InitDone`), any live record older than 5 minutes whose pod wasn't listed, or was listed as `Failed`/`Succeeded`, is closed. The watcher re-lists the same way after any reconnect, so deletions missed while it was down are caught then.
- **Then react to changes.** When a pod is deleted, or reaches `Failed`/`Succeeded`, its record is closed after a grace period (`CLOSE_GRACE`, 30 seconds).
- **Leave connected pods to crash detection.** `close_if_gone` skips a record that's no longer live, one with a registered connection (crash detection closes the same records but also tells the model), and one whose pod is still running in Kubernetes.
- **Close quietly.** Commands are marked lost and terminals and the pod closed, publishing the usual events, with no notice to the model: that would move each conversation to the top of the sidebar.
- **Tell the model when a language server stops.** A server pod that stops on its own (out of memory, crashed) is reported to its conversation once (`lsp::pods::note_server_stop`); one smelt deleted, or already stopped when listed, isn't.
- **Real servers only.** The browser harness never runs it, since it shares the dev database but uses the test namespace. The 5-minute cut-off also keeps one instance from closing another's brand-new rows.

`stop_pod(pod_id)` (`sandbox::stop_pod_for_user`) tears a pod down whether or not it has terminals, the way a crash does: running commands are marked lost and terminals closed. The model is then told in one notice ("The user stopped sandbox pod N…"), saved between turns. It doesn't wake the model. `get_live_pod_conversations` lists the conversations with a live pod, for the sidebar's dots.

## The browsing panel's own live channel

Unlike every other panel above, the browsing panel's live frames are **not** part of `ConversationEvent` — they're frequent, ephemeral, UI-only data (a base64 JPEG per screen update) that shouldn't crowd out task/message updates in that shared bounded broadcast channel, so they get a dedicated per-conversation channel instead, in `src/browsing.rs`, with its server functions in `api::browsing`:

```rust
#[get("/api/conversations/{id}/browsing/frames")]
pub async fn subscribe_browser_frames(id: i64) -> ServerFnResult<ServerEvents<browsing::BrowserFrame>>;

#[post("/api/conversations/{id}/browsing/input")]
pub async fn send_browser_input(id: i64, event: browsing::BrowserInputEvent) -> ServerFnResult<()>;

#[post("/api/conversations/{id}/browsing/navigate")]
pub async fn navigate_browser(id: i64, address: String) -> ServerFnResult<()>;
```

`navigate_browser` is the address bar's endpoint. With no scheme typed, `browsing::normalize_address` picks one as Chromium's omnibox does (SME-184): `http://` for an IP literal, an explicit port other than 443, a single-label name or a local-only suffix (`.localhost`, `.local`, `.lan`, `.internal`, `.test`, `.home.arpa`), so `localhost:5173` reaches a dev server in the sandbox, and `https://` for anything else; then it runs the same `browsing::navigate` the model's `browser_navigate` uses, so the same URL checks apply identically. The address bar learns the resulting URL the same way it learns about every other navigation: `BrowsingUrlUpdate`, published whenever the session page's main-frame URL changes (`frameNavigated`, plus `navigatedWithinDocument` for `pushState`/fragment changes). A failed load reports the address that failed, not Chrome's `chrome-error://` page.

`subscribe_browser_frames` starts the real CDP screencast (`Page.startScreencast`) the moment the *first* viewer subscribes and stops it (`Page.stopScreencast`) when the *last* one disconnects — reference-counted, via a `Drop`-based guard inside `browsing::FrameSubscription`. The count only flips a flag; one long-lived task per session (`browsing::run_screencast`) issues every start/stop command itself, so a stop meant for a departed viewer can never land after a new viewer's start. Like every stream here (`subscribe_conversation_events`, `subscribe_app_events`), it's built with `ServerEvents::from_stream`, not `ServerEvents::new`, and that choice matters: `new` runs its closure as a detached task feeding an unbounded queue, so a closed connection is never noticed (the viewer would never be released) and a slow reader would build an unbounded backlog of frames. With `from_stream` the response body *pulls* each frame only when the connection can take it: a slow viewer just gets the newest frame on its next pull, and a closed connection drops the stream and the viewer with it. Frames travel over a `watch` channel holding only the latest frame, not a broadcast channel: Chrome only sends a frame when something on screen changes, so a viewer joining a running screencast on a still page (a second tab, a reload) would otherwise wait forever. With `watch`, a new viewer gets the current frame at once. `send_browser_input` forwards one mouse/keyboard event (in the frame's own fixed pixel space — the session's viewport is pinned to match the screencast bounds exactly, so no scale-factor lookup is needed) to the real page via `Input.dispatchMouseEvent`/`dispatchKeyEvent`/`insertText`. The panel sends these one request at a time, in order (see [frontend.md](../frontend.md#the-live-browsing-panel)), since separately issued requests can overtake each other. There's no locking between this and the model's own tool-driven `browser_click`/`browser_fill`/... calls — both act on the same real page, and CDP just serializes whichever commands arrive.
