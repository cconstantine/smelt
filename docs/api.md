# API — Dioxus Server Functions

There is no hand-rolled Axum API layer and no hand-written browser fetch client. Every endpoint is a plain async function in `src/api/chat.rs` decorated with `#[get(...)]` or `#[post(...)]` (from `dioxus::prelude`, available under the `fullstack` feature). The macro generates two different bodies for the same function signature:

- **Server build** (`feature = "server"`): registers a real Axum route (via `inventory::submit!`) and runs your actual function body.
- **Web build** (`feature = "web"`): replaces the body with a transparent HTTP call to that route, JSON-encoding the arguments and decoding the response.

So a frontend component calls `get_conversations()` exactly like calling a local async function — there is no separate `frontend/api.rs` fetch-helper file to keep in sync with the server routes.

```rust
#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations().await.map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    db::list_messages(id).await.map_err(ServerFnError::new)
}
```

Path parameters (`{id}`) map directly onto a same-named function argument. `ServerFnResult<T>` is `Result<T, ServerFnError>` — return it from every server function; the macro asserts on this at compile time.

## Sending a message: `send_message`

`send_message(id, content) -> ServerFnResult<()>` is an ordinary request (`start_turn` in `api::chat`):
1. It checks the conversation exists (`conversation not found` otherwise).
2. It ends any pause from an earlier stop.
3. It starts the user's turn in a background task and returns, without waiting for the turn.

The turn is `api::chat::run_turn`, a *loop* (capped at `MAX_TURNS`), not one Anthropic call: whenever the model's `stop_reason` is `"tool_use"`, it runs each tool (`anthropic::tools::execute`), saves the `ToolResult` turn, and loops again. Everything it does reaches the browser on the **conversation's event stream** (`subscribe_conversation_events`, below), in every tab watching the conversation, not only the one that sent:
- **`MessagesAppended`** for each message as it's saved: the user's own message first (which replaces the sender's optimistic copy), then each tool call, tool result and reply.
- **`ReplyReset {}`** when each model call starts, and **`ReplyDelta { text }`** as its text streams. So each model call is its own streaming bubble, and a multi-step reply no longer runs together.
- **`TurnState`** when the turn starts and ends.
- **`TurnError { message }`** if the user's turn fails or is stopped (`TURN_STOPPED`). The message is the error's own text (`chat_error_text`), without `ServerFnError`'s "error running server function: … (details: None)" wrapper.

**Why not stream the reply on the request itself**, as it used to (`ServerEvents<ChatEvent>`)? Because that held a second connection open per tab for the whole reply. Over plain HTTP/1.1 a browser allows 6 connections per host across all tabs, so with a few tabs open a new tab couldn't load and a click (Stop) waited for the reply to end (see [SME-27](https://linear.app/smelt-agent/issue/SME-27)). Now a tab holds one connection, plus the browsing panel's while that's open. It also means every tab sees a reply live, and a tab that (re)connects mid-reply shows the text so far (`get_reply_in_progress`, from the in-memory `REPLIES_IN_PROGRESS`, cleared when a call starts, when its reply is saved, and when the turn ends).

Every turn request carries a **system prompt**: `system_prompt(&prompt_environment(pool).await)` in `api::chat`. It has two parts:
- **The fixed base prompt**, `src/api/system_prompt.md`, compiled in with `include_str!`. It covers smelt as a coding agent, how the sandbox works (create the pod first, the `sandbox` user in `/home/sandbox`, `sudo`, what's lost when a pod ends), background commands and their notifications, the file tools, the web tools, working style, and that replies are shown as plain text rather than rendered markdown.
- **An environment section** built from the database and clock for each request: today's date (UTC), the model, the configured volumes with their mount paths, and the configured MCP servers. A failed database read leaves its line out and is logged, rather than failing the turn.

The prompt only changes when the date or configuration does. `test_system_prompt_only_names_real_tools` fails if the base prompt names a tool that doesn't exist, so renaming a tool means updating the prompt too. Compaction's summarization call keeps its own `COMPACTION_SYSTEM_PROMPT`.

Requests carry `thinking: {"type": "adaptive"}` by default (`ANTHROPIC_THINKING=0` to turn it off — see [setup.md](setup.md)), so an assistant turn's `content` can start with a `ContentBlock::Thinking { thinking, signature }` block ahead of any `Text`/`ToolUse` blocks — `run_turn` persists and replays it exactly like any other block, uninterpreted; the frontend renders it as a collapsed-by-default `<details>` (see `frontend/pages/chat.rs`'s `render_block_element`).

If a request fails with Ollama's specific "error parsing tool call" 500 (its Anthropic-compat shim, at least for `gpt-oss` models, doesn't always turn a model's raw output into valid tool-call JSON — either because thinking's reasoning landed in the same text as the call, or the model just wrote invalid JSON on its own), `run_turn_bounded` retries: first with `thinking` dropped, then up to `TOOL_CALL_PARSE_RETRIES` further plain regenerations, since a local model's next sampling pass often doesn't repeat the same malformed output. Gives up and surfaces the error once that's exhausted. Any other failure propagates from `run_turn_bounded` immediately. See `is_ollama_thinking_tool_call_corruption`.

One layer down, `anthropic::stream::stream_anthropic_message` retries a briefly unavailable provider on its own: an HTTP 429, 503 or 529 gets two more attempts, 1s and then 3s later (`RETRY_DELAYS`), before anything has streamed. Any other status fails at once. A failure reads `model provider error {status}: {message}`, where the message is the provider's own (`provider_error_message` unwraps the JSON error body, including a nested one), not the raw response.

`run_turn_bounded` refuses a conversation that doesn't exist with a plain `conversation not found` (checked right after taking the conversation's lock), rather than letting the insert fail on a foreign key. `get_messages` returns the same error, which the chat page uses to show "This conversation doesn't exist" instead of a message box. The new message is saved *before* the credential check, so with no credentials configured the message is kept and only the reply fails.

**Some rows are no longer guaranteed to be persisted by the request that "sent" them.** A `run_async` tool call returns immediately, but the task it started can later push its own turns onto the conversation via `run_turn` from a background task — with no `send_message` call in flight at all. Those rows are stored exactly like any other turn (see [models.md](models.md)), just written outside the request/response cycle a naive reading of this API might assume. See "Live conversation events" below for how a browser tab learns about them.

## Live conversation events

Seven server functions exist purely to support live-updating panels (the background-tasks panel via `run_async`, the sandbox panel — see SME-10 — the context-usage indicator/detail view — see SME-18 — the todo panel via `todowrite`/`todoread` — see SME-20 — and the browsing-session panel — see SME-22) and turns pushed from outside a request:

```rust
#[get("/api/conversations/{id}/tasks")]
pub async fn get_tasks(id: i64) -> ServerFnResult<Vec<anthropic::tools::TaskSummary>>;

#[get("/api/conversations/{id}/todos")]
pub async fn get_todos(id: i64) -> ServerFnResult<Vec<anthropic::tools::TodoItem>>;

#[get("/api/conversations/{id}/sandbox")]
pub async fn get_sandbox_state(id: i64) -> ServerFnResult<SandboxSnapshot>;

#[get("/api/conversations/{id}/context-usage")]
pub async fn get_context_usage(id: i64) -> ServerFnResult<ContextUsageSnapshot>;

#[get("/api/conversations/{id}/context-detail")]
pub async fn get_context_detail(id: i64) -> ServerFnResult<ContextDetailSnapshot>;

#[get("/api/conversations/{id}/browsing")]
pub async fn get_browsing_state(id: i64) -> ServerFnResult<browsing::BrowsingState>;

#[get("/api/conversations/{id}/events")]
pub async fn subscribe_conversation_events(id: i64) -> ServerFnResult<ServerEvents<ConversationEvent>>;
```

`get_tasks` is a thin, one-shot wrapper around `anthropic::tools::snapshot_tasks` — every task started via `run_async` in that conversation, with its current status. `get_todos` is the same one-shot shape for the todo panel: the conversation's current todo list, as last set by `todowrite` (`db::get_conversation_todos` — empty if never called). `get_sandbox_state` is the same shape for the sandbox panel: every pod and terminal currently live in the conversation, each terminal hydrated with its `HISTORY_LIMIT` most recent commands (oldest first), each with the last 200 lines per stream, merged back into one true chronological `output` (stdout and stderr are fetched/capped independently so one stream can't crowd the other out of the window, then re-sorted by `seq` — see `fetch_command_summary` — so the panel doesn't show "all stdout, then all stderr"). Older history beyond the limit isn't duplicated here, it's still reachable through the model's own `list_commands`/`read_terminal_output` tools:

```rust
pub struct SandboxOutputLine { stream: String, data: String }
pub struct SandboxCommandSummary { command_id: String, command: String, status: String, exit_code: Option<i32>, output: Vec<SandboxOutputLine> }
pub struct SandboxTerminalSummary { terminal_id: i64, pod_id: i64, status: String, commands: Vec<SandboxCommandSummary> }
pub struct SandboxPodSummary { pod_id: i64, status: String, terminals: Vec<SandboxTerminalSummary> }
pub struct SandboxSnapshot { pods: Vec<SandboxPodSummary> }
```

`get_context_usage` is the always-visible indicator's one-shot pull — the last real `usage` numbers persisted for this conversation (`None` if no turn has completed yet) plus the configured model's context window. `get_context_detail` is the click-through view: the same usage numbers, plus the exact system prompt a turn sends (from the same `system_prompt`/`prompt_environment` pair), every available tool's full definition, and the current message count — reconstructed from current state each call, not a stored snapshot of some specific past request:

```rust
pub struct ContextUsageSnapshot { usage: Option<anthropic::TokenUsage>, context_window: u32 }
pub struct ContextDetailSnapshot { system: Option<String>, tools: Vec<anthropic::ToolDefinition>, message_count: usize, usage: Option<anthropic::TokenUsage>, context_window: u32 }
```

`get_browsing_state` is the browsing panel's own one-shot check — is a session open right now, and on what URL (`browsing::current_url`) — so the panel can show an idle state instead of trying to subscribe to a frame stream that doesn't exist yet, and fill its address bar:

```rust
pub struct BrowsingState { session_open: bool, url: Option<String> }
```

`subscribe_conversation_events` is the conversation's `ServerEvents` stream. It isn't scoped to one request: a browser tab opens it once per viewed conversation and keeps it open for as long as that conversation is selected, forwarding whatever `events::subscribe(id)` yields:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ConversationEvent {
    TaskUpdate { task_id: String, tool: String, status: String, stream: Option<String>, latest_output: Option<String> },
    MessagesAppended { messages: Vec<Message> },
    SandboxPodUpdate { pod_id: i64, status: String, terminated: bool },
    SandboxTerminalUpdate { pod_id: i64, terminal_id: i64, status: String, terminated: bool },
    SandboxCommandUpdate { terminal_id: i64, command_id: String, command: Option<String>, status: String, exit_code: Option<i32>, stream: Option<String>, latest_output: Option<String> },
    NotificationDeliveryFailed { detail: String },
    ContextUsageUpdate { usage: anthropic::TokenUsage, context_window: u32 },
    TodoListUpdate { items: Vec<anthropic::tools::TodoItem> },
    BrowsingSessionUpdate { open: bool },
    BrowsingUrlUpdate { url: String },
    PodsChanged {},
    TurnState { running: bool },
}
```

`MessagesAppended` is a struct variant, not `MessagesAppended(Vec<Message>)`: the enum is internally tagged, and serde can't write a tuple variant holding a list that way. It used to be one, and every send failed silently (`wire_tests::test_every_event_round_trips_through_json` now serializes one of each variant). The stream is built with `ServerEvents::from_stream` over the broadcast receiver, for the same reason as the frame stream below: a closed connection drops the subscription. `NotificationDeliveryFailed` is published when a turn fired with no request in flight fails: `wake_conversation` (a terminal command finished) and a `run_async` task's output or completion notification.

`TaskUpdate`/`Sandbox*`/`ContextUsageUpdate`/`TodoListUpdate`/`BrowsingSessionUpdate`/`BrowsingUrlUpdate` are all ephemeral UI telemetry (never persisted as such, regenerable at any time from `get_tasks`/`get_sandbox_state`/`get_context_usage`/`get_todos`/`get_browsing_state` — `ContextUsageUpdate`'s own numbers *are* separately persisted, in `conversation_context_usage`, precisely so `get_context_usage` can regenerate it, and `TodoListUpdate`'s are likewise persisted in `conversation_todos`); `MessagesAppended` is a live-delivery notification for rows `run_turn` already persisted — whether that `run_turn` call came from a live `send_message` or from a background task's own push. `SandboxPodUpdate`/`SandboxTerminalUpdate` fire on create/terminate (a `terminated: true` update means the frontend should *remove* that pod/terminal, not just relabel it — unlike a finished task, which the task panel keeps showing); `SandboxCommandUpdate` follows the exact same "started/one output line/finished" pattern `TaskUpdate` already uses, with `command` only populated on the "started" event. `ContextUsageUpdate` publishes once per completed real turn, right after `run_turn_bounded` persists that turn's usage. `TodoListUpdate` publishes on every `todowrite` call and always carries the *complete* current list (never a partial diff — `todowrite` itself is a whole-list replace, no per-item ids), so the frontend just overwrites its signal wholesale rather than merging like `TaskUpdate` requires. `BrowsingSessionUpdate` publishes from `open_browser_session`/`close_browser_session`, so the panel shows/hides reactively rather than only checking on conversation (re)select. Since the underlying `broadcast` channel has no replay, the frontend does a one-shot `get_messages`/`get_tasks`/`get_sandbox_state`/`get_context_usage`/`get_todos`/`get_browsing_state` reconciliation pull on connect/reconnect to cover anything published before it subscribed — see `architecture.md`.

`PodsChanged` is the app-wide `AppEvent::PodsChanged` (below), relayed on every conversation's stream: the server merges the app-wide channel into each subscription (`conversation_event_stream`). A chat tab therefore needs no second always-open connection for the sidebar's pod dots. Over plain HTTP/1.1 a browser allows only 6 connections per host, shared by every tab, and each chat tab already holds one stream (two while a reply streams). `TurnState` is published when a conversation's first turn starts and when its last one ends, including turns nobody started from a tab (a finished command, a background task, another tab). `get_turn_state` is its snapshot for reconnects.

### Stopping a turn

`stop_turn(id)` ends the conversation's running turn, and any turn queued behind it:
- **How:** each conversation has a stop counter (`TURN_STOPS`, a `watch` channel). `run_turn_bounded` notes the counter's value before waiting for the turn lock, then runs the turn (`run_turn_body`) in a `select!` against it changing. A stop drops the turn wherever it's waiting (the model's stream, a tool call, a compaction), which releases the lock. Turns that start after the stop aren't affected.
- **Left alone:** the pod, its terminals and running commands.
- **What's left behind:** a reply that was streaming isn't saved, and tool calls the model had made but that hadn't returned have no result. `answer_unfinished_tool_calls` fixes that when each request is built: every tool call without a result gets an error result (`UNFINISHED_TOOL_CALL`), at the start of the next user message, or in a new one at the end. It isn't saved, since notices can already follow the dangling call. This also repairs a turn cut off by a server restart.
- **The pause:** a stop also pauses the conversation (`PAUSED`, in memory). While paused, `wake_conversation` does nothing (a finished command's notice stays pending, and the next turn drains it), and a background task's notice is saved with `save_notice_between_turns` instead of starting a turn. `send_message` ends the pause. Without it, a command still running would restart the model seconds after the user stopped it.

`save_notice_between_turns(pool, id, text)` saves a `user` notice only while no turn holds the conversation's lock, so it can't land between a tool call and its result (an order the API rejects). It's used by the pod-crash path and the user stopping a pod.

### App-wide events and pods

`subscribe_app_events` is a third always-open stream, of `AppEvent` (`src/events.rs`), for views that span conversations. Today it has a single event, `PodsChanged`, published when a pod is created (`create_pod`), goes away (`force_terminate_pod`: a model's terminate, a user's stop, a crash), or is torn down with its conversation (`delete_conversation`). Only the pods page subscribes to it directly; chat tabs get it relayed as `ConversationEvent::PodsChanged` (see above).

`get_pods` returns a `PodOverview` for every live pod:
- **From the database** (`db::list_live_pods`): its conversation, start time, and live terminal count.
- **Activity** (`pod_activity`): busy while any terminal has a running command; otherwise idle since the latest of the last command finishing, the conversation's last message, and the pod starting.
- **From Kubernetes:** the pod object's phase and limits (`sandbox::pod_details`), and live use from one metrics list (`sandbox::pod_metrics_list`, `metrics.k8s.io/v1beta1`).
  - Reading metrics needs `get`/`list` on `pods` in the `metrics.k8s.io` group (`k8s/smelt-park-rbac.yaml`). Without it, or if the metrics call fails for any other reason, every pod's `usage` is `None` and the rest of the view still works.
  - Quantities are parsed by `parse_cpu_nanocores`/`parse_memory_bytes`, and CPU is summed across containers in nanocores.
- **`observed_at`:** the database's `now()`, so the page measures ages on the database's clock rather than the browser's.

**Records are kept in step with the cluster.** A pod can vanish while smelt isn't connected to it: a cluster rebuild, a pod deleted outside smelt, one that died while smelt was down. Crash detection only notices a pod with a live connection. So `main()` starts `sandbox::watch_pods`, a Kubernetes watch on smelt's namespace (`kube::runtime::watcher`), which works in this order:
- **Subscribe first, then list.** When the watch's full listing arrives (`Init`, `InitApply`…, `InitDone`), any live record older than 5 minutes whose pod wasn't listed, or was listed as `Failed`/`Succeeded`, is closed. The watcher re-lists the same way after any reconnect, so deletions missed while it was down are caught then.
- **Then react to changes.** When a pod is deleted, or reaches `Failed`/`Succeeded`, its record is closed after a grace period (`CLOSE_GRACE`, 30 seconds).
- **Leave connected pods to crash detection.** `close_if_gone` skips a record that's no longer live, one with a registered connection (crash detection closes the same records but also tells the model), and one whose pod is still running in Kubernetes.
- **Close quietly.** Commands are marked lost and terminals and the pod closed, publishing the usual events, with no notice to the model: that would move each conversation to the top of the sidebar.
- **Real servers only.** The browser harness never runs it, since it shares the dev database but uses the test namespace. The 5-minute cut-off also keeps one instance from closing another's brand-new rows.

`stop_pod(pod_id)` (`sandbox::stop_pod_for_user`) tears a pod down whether or not it has terminals, the way a crash does: running commands are marked lost and terminals closed. The model is then told in one notice ("The user stopped sandbox pod N…"), saved between turns. It doesn't wake the model. `get_live_pod_conversations` lists the conversations with a live pod, for the sidebar's dots.

### The browsing panel's own live channel

Unlike every other panel above, the browsing panel's live frames are **not** part of `ConversationEvent` — they're frequent, ephemeral, UI-only data (a base64 JPEG per screen update) that shouldn't crowd out task/message updates in that shared bounded broadcast channel, so they get a dedicated per-conversation channel instead, in `src/browsing.rs` itself:

```rust
#[get("/api/conversations/{id}/browsing/frames")]
pub async fn subscribe_browser_frames(id: i64) -> ServerFnResult<ServerEvents<browsing::BrowserFrame>>;

#[post("/api/conversations/{id}/browsing/input")]
pub async fn send_browser_input(id: i64, event: browsing::BrowserInputEvent) -> ServerFnResult<()>;

#[post("/api/conversations/{id}/browsing/navigate")]
pub async fn navigate_browser(id: i64, address: String) -> ServerFnResult<()>;
```

`navigate_browser` is the address bar's endpoint. A bare host gets `https://` (`browsing::normalize_address`); then it runs the same `browsing::navigate` the model's `browser_navigate` uses, so the scheme check and SSRF guard apply identically. The address bar learns the resulting URL the same way it learns about every other navigation: `BrowsingUrlUpdate`, published whenever the session page's main-frame URL changes (`frameNavigated`, plus `navigatedWithinDocument` for `pushState`/fragment changes). A failed load reports the address that failed, not Chrome's `chrome-error://` page.

`subscribe_browser_frames` starts the real CDP screencast (`Page.startScreencast`) the moment the *first* viewer subscribes and stops it (`Page.stopScreencast`) when the *last* one disconnects — reference-counted, via a `Drop`-based guard inside `browsing::FrameSubscription`. The count only flips a flag; one long-lived task per session (`browsing::run_screencast`) issues every start/stop command itself, so a stop meant for a departed viewer can never land after a new viewer's start. Unlike `send_message`/`subscribe_conversation_events`, it's built with `ServerEvents::from_stream`, not `ServerEvents::new`, and that choice matters: `new` runs its closure as a detached task feeding an unbounded queue, so a closed connection is never noticed (the viewer would never be released) and a slow reader would build an unbounded backlog of frames. With `from_stream` the response body *pulls* each frame only when the connection can take it: a slow viewer just gets the newest frame on its next pull, and a closed connection drops the stream and the viewer with it. Frames travel over a `watch` channel holding only the latest frame, not a broadcast channel: Chrome only sends a frame when something on screen changes, so a viewer joining a running screencast on a still page (a second tab, a reload) would otherwise wait forever. With `watch`, a new viewer gets the current frame at once. `send_browser_input` forwards one mouse/keyboard event (in the frame's own fixed pixel space — the session's viewport is pinned to match the screencast bounds exactly, so no scale-factor lookup is needed) to the real page via `Input.dispatchMouseEvent`/`dispatchKeyEvent`/`insertText`. The panel sends these one request at a time, in order (see [frontend.md](frontend.md#the-live-browsing-panel)), since separately issued requests can overtake each other. There's no locking between this and the model's own tool-driven `browser_click`/`browser_fill`/... calls — both act on the same real page, and CDP just serializes whichever commands arrive.

## Current endpoints

| Function | Method + path | Notes |
|---|---|---|
| `get_conversations` | `GET /api/conversations` | ordered by `updated_at DESC` |
| `create_conversation` | `POST /api/conversations` | default title |
| `get_messages` | `GET /api/conversations/{id}/messages` | ordered by `created_at ASC` |
| `send_message` | `POST /api/conversations/{id}/messages` | starts the user's turn and returns; the reply arrives on the conversation's event stream, see above |
| `get_tasks` | `GET /api/conversations/{id}/tasks` | one-shot snapshot of `run_async` tasks for this conversation |
| `get_todos` | `GET /api/conversations/{id}/todos` | one-shot snapshot of the current todo list, see above |
| `get_sandbox_state` | `GET /api/conversations/{id}/sandbox` | one-shot snapshot of every pod/terminal for this conversation, see above |
| `get_context_usage` | `GET /api/conversations/{id}/context-usage` | one-shot snapshot for the always-visible context-usage indicator, see above |
| `get_context_detail` | `GET /api/conversations/{id}/context-detail` | one-shot snapshot for the context-usage detail view, see above |
| `get_browsing_state` | `GET /api/conversations/{id}/browsing` | one-shot check for whether a browsing session is open, and its URL, see above |
| `subscribe_browser_frames` | `GET /api/conversations/{id}/browsing/frames` | live screencast frame stream for the browsing panel, see above |
| `send_browser_input` | `POST /api/conversations/{id}/browsing/input` | forwards one live-panel mouse/keyboard event, see above |
| `navigate_browser` | `POST /api/conversations/{id}/browsing/navigate` | the live panel's address bar, see above |
| `subscribe_conversation_events` | `GET /api/conversations/{id}/events` | always-open live stream, see above |
| `stop_turn` | `POST /api/conversations/{id}/stop` | the Stop button: end the running turn and pause, see above |
| `get_turn_state` | `GET /api/conversations/{id}/turn` | whether a turn is running, for the Stop button on (re)connect |
| `get_reply_in_progress` | `GET /api/conversations/{id}/reply` | the reply streamed so far, for a tab that (re)connects mid-reply |
| `get_pods` | `GET /api/pods` | every live pod with status, activity, limits and usage, see above |
| `stop_pod` | `POST /api/pods/{pod_id}/stop` | the user stopping a pod, see above |
| `get_live_pod_conversations` | `GET /api/pods/conversations` | conversations with a live pod, for the sidebar |
| `subscribe_app_events` | `GET /api/app-events` | always-open app-wide stream (`AppEvent`), for the pods page |
| `delete_conversation` | `DELETE /api/conversations/{id}` | hard delete; cascades to the conversation's messages (`ON DELETE CASCADE`); also tears down its sandboxes and browsing session, stops its `run_async` tasks, and drops its event channel and turn lock; deleting a nonexistent id is not an error |

Not yet implemented (straightforward mechanical additions when needed): rename a conversation, concurrent-send guarding.
