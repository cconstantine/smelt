# API — Dioxus Server Functions

There is no hand-rolled Axum API layer and no hand-written browser fetch client. Every endpoint is a plain async function in one of `src/api/`'s modules (`chat`, `browsing`, `git`, `language_servers`, `mcp`, `pods`, `sandbox_volumes`) decorated with `#[get(...)]`, `#[post(...)]` or `#[delete(...)]` (from `dioxus::prelude`, available under the `fullstack` feature). The macro generates two different bodies for the same function signature:

- **Server build** (`feature = "server"`): registers a real Axum route (via `inventory::submit!`) and runs your actual function body.
- **Web build** (`feature = "web"`): replaces the body with a transparent HTTP call to that route, JSON-encoding the arguments and decoding the response.

So a frontend component calls `get_conversations()` exactly like calling a local async function — there is no separate `frontend/api.rs` fetch-helper file to keep in sync with the server routes.

```rust
#[get("/api/conversations")]
pub async fn get_conversations() -> ServerFnResult<Vec<Conversation>> {
    db::list_conversations(db::get()).await.map_err(ServerFnError::new)
}

#[get("/api/conversations/{id}/messages")]
pub async fn get_messages(id: i64) -> ServerFnResult<Vec<Message>> {
    if !db::conversation_exists(db::get(), id).await.map_err(ServerFnError::new)? {
        return Err(ServerFnError::new("conversation not found"));
    }
    db::list_messages(db::get(), id).await.map_err(ServerFnError::new)
}
```

Path parameters (`{id}`) map directly onto a same-named function argument. `ServerFnResult<T>` is `Result<T, ServerFnError>` — return it from every server function; the macro asserts on this at compile time. Server functions pass `db::get()` as the pool (see [database.md](database.md)).

## Routes that aren't server functions

`main.rs`'s `build_router` adds a few things around the Dioxus app:
- **`GET /oauth/mcp-callback/{id}`** → `mcp_oauth::callback_handler`, a plain Axum route. The OAuth provider sends the user's browser here, and it has to answer with a real redirect back into the app, not JSON. See [mcp.md](mcp.md#oauth).
- **`request_guard::guard`**, a layer over every route (SME-51). smelt has no login, so it refuses what another site's page could send on the user's behalf. A request that isn't `GET`/`HEAD`/`OPTIONS` is refused when `Sec-Fetch-Site` says it came from another site (`same-site` included: a sandbox preview is a sibling origin), or, without that header, when `Origin` names another host. Once `SMELT_ALLOWED_HOSTS` is set, a `Host` that isn't one of those names or `SMELT_BASE_URL`'s host is refused too (IP addresses and `localhost` always pass). A request with neither header (curl, a script) is let through.

The sandbox preview proxy (`src/preview.rs`, SME-42) isn't on this router at all: it has a listener of its own, `SMELT_PREVIEW_ADDR` (default `0.0.0.0:8181`), because the preview's address is all in `Host` and `dx serve`'s dev proxy replaces `Host`. See [setup.md](setup.md).

## Sending a message: `send_message`

`send_message(id, content) -> ServerFnResult<()>` is an ordinary request (`start_turn` in `api::chat`):
1. It checks the conversation exists (`conversation not found` otherwise).
2. It ends any pause from an earlier stop.
3. It starts the user's turn in a background task and returns, without waiting for the turn.

Turns in one conversation don't overlap: each takes the conversation's lock (`conversation_lock`) and queues behind whatever turn is running, whether a second send or a notice (a finished command, a clone, a Docker restart).

The turn is `turn::run_turn`, a *loop* (capped at `MAX_TURNS`), not one Anthropic call: whenever the model's `stop_reason` is `"tool_use"`, it runs each tool (`anthropic::tools::execute`), saves the `ToolResult` turn, and loops again. Everything it does reaches the browser on the **conversation's event stream** (`subscribe_conversation_events`, below), in every tab watching the conversation, not only the one that sent:
- **`MessagesAppended`** for each message as it's saved: the user's own message first (which replaces the sender's optimistic copy), then each tool call, tool result and reply.
- **`ReplyReset {}`** when each model call starts (and again on each retry, whose text replaces the failed attempt's), and **`ReplyDelta { text, offset }`** as its text streams. So each model call is its own streaming bubble, and a multi-step reply no longer runs together. `offset` is the reply's length in bytes before `text`, so a tab that has just fetched the reply so far skips what it already has (SME-51).
- **`TurnState`** when the turn starts and ends.
- **`TurnError { message }`** if the user's turn fails or is stopped (`TURN_STOPPED`). The message is the error's own text (`chat_error_text`), without `ServerFnError`'s "error running server function: … (details: None)" wrapper. A failure (not a stop) is also kept in memory (the conversation's `turn::state` entry) until the user writes again, and `get_turn_error` serves it to a tab that connects afterwards. It's kept only if no newer turn has started since the failed one did (a turn generation per conversation, bumped by the user sending and by every turn starting), so a turn that fails after the user already sent the next message doesn't leave its error under that message's reply (SME-91). A "Work on a repo" that couldn't start the sandbox for a repo already checked out is shown the same way (`show_conversation_error`).

**A tool result is at most `MAX_TOOL_RESULT_CHARS` (30,000) characters**, for every tool, MCP or native, success or error: `execute` cuts anything longer and appends a line saying how much was cut and to narrow the request (SME-76). The rest is dropped, not saved anywhere. `read_file` and `read_terminal_output` stay under it on their own (`fit_lines`): a line over 2,000 characters is cut, and a read stops at a whole line with `truncated: true` and the `next_offset` to read from, instead of being cut mid-line by `execute`.

**Why not stream the reply on the request itself**, as it used to (`ServerEvents<ChatEvent>`)? Because that held a second connection open per tab for the whole reply. Over plain HTTP/1.1 a browser allows 6 connections per host across all tabs, so with a few tabs open a new tab couldn't load and a click (Stop) waited for the reply to end (see [SME-27](https://linear.app/smelt-agent/issue/SME-27)). Now a tab holds one connection, plus the browsing panel's while that's open. It also means every tab sees a reply live, and a tab that (re)connects mid-reply shows the text so far (`get_reply_in_progress`, from the conversation's in-memory `turn::state` entry, cleared when a call starts, when its reply is saved, and when the turn ends). A delta is added there and published under the same lock `get_reply_in_progress` reads through (`relay_reply_delta`), so the fetched text and the offsets that follow agree.

Every turn request carries a **system prompt**: `system_prompt(&prompt_environment(pool, conversation_id).await)` in `api::chat`. It's per conversation, and has three parts:
- **The fixed base prompt**, `src/turn/system_prompt.md`, compiled in with `include_str!`. After a line on smelt as a coding agent, its sections are: Your sandbox, Running commands, Files, Language servers, Docker, Git, The web, Servers you run, Working style, and How your replies are shown (plain text, not rendered markdown).
- **An environment section** built from the database and clock for each request: today's date (UTC), the model, the configured volumes with their mount paths, the configured MCP servers, and the conversation's repositories (path, URL, and the branch, "still cloning" or "clone failed", with its `AGENTS.md` files, the loaded ones marked `(loaded)`). A failed database read leaves its part out and is logged, rather than failing the turn.
- **The loaded project instructions**: each `AGENTS.md` the model has loaded, in full (`git::render_project_instructions`).

The prompt changes with the date and configuration, and within a conversation when its repos or loaded instructions change. `test_system_prompt_only_names_real_tools` fails if the base prompt names a tool that doesn't exist, so renaming a tool means updating the prompt too. Compaction's summarization call keeps its own `COMPACTION_SYSTEM_PROMPT`.

Requests carry `thinking: {"type": "adaptive"}` unless the conversation's model has it off (its setting on `/providers`, or its Ollama server saying it can't think — see [setup.md](setup.md#model-providers)), so an assistant turn's `content` can start with a `ContentBlock::Thinking { thinking, signature }` block ahead of any `Text`/`ToolUse` blocks — `run_turn` persists and replays it exactly like any other block, uninterpreted; the frontend renders it as a collapsed-by-default `<details>` (see `frontend/pages/chat/transcript.rs`'s `render_block_element`).

**Content blocks are accumulated by their `index`** (SME-112). Anthropic and Ollama close each block before starting the next, but llama.cpp's `llama-server` opens thinking, text and each tool call without closing any, and sends every `content_block_stop` at the end of the reply. `stream_anthropic_message` keeps every open block (`Blocks`), and the reply's content is in index order, whatever order the stops come in; a block still open when the stream ends is kept. Before this, a tool call's start replaced the text before it, so llama.cpp's commentary between tool calls showed while streaming and was gone once the message saved. A `content_block_start` for `text` or `thinking` still opens nothing (its first delta does), so a thinking block Anthropic sends with its text omitted is still dropped. llama.cpp's thinking blocks are now saved with an empty signature and replayed.

**A provider that refuses replayed thinking** (SME-93). Under Anthropic's preserved thinking a thinking block is bound to the `system` prompt, the `tools` and every message before it, and smelt's system prompt changes within a conversation (above), as does its tool list (MCP servers). Accounts created on or after 2026-08-31 refuse a request that replays a block whose prefix changed, with a 400 saying it's "bound to a different conversation". `stream_anthropic_message` retries such a request once more before anything has streamed: with thinking on, under the `thinking-binding-controls-2026-08-01` beta with `thinking.block_binding.prefix_mismatch_behavior: "drop_block"` (the API drops what it can't accept); with thinking off, or if that retry is refused too, with the history's thinking blocks removed. Which form worked is remembered per provider (base URL and credential, in memory; enforcement is per account), so later requests go out that way without the extra 400; a restart forgets it. A provider that never sends that 400 (older accounts, Ollama, other servers) never gets the beta or the field: setting the field would opt an older account into the check. Drops the API reports in `message_start`'s `input_transformations` are logged. The earlier reasoning is lost either way; keeping it means sending prompt and tool changes append-only (SME-98).

### Which model a turn runs on

Each conversation has its own provider and model (`conversations.provider_id`/`model`, SME-72). `run_turn_body` resolves them into a `providers::TurnModel` (endpoint, model id, thinking, context window) the first time it needs to call the model: after the new message and any pending notices are saved, so both survive a turn that can't run, and not at all when a wake-up finds nothing to do. The whole turn, its tool loop and any compaction use that one snapshot; a change made meanwhile applies from the next turn.

**The reply budget** (SME-111): each request's `max_tokens` is `turn::reply_budget`, computed per request and never stored: the smallest of the model's output cap (where known), the room the request leaves in the context window less `COMPACTION_SAFETY_BUFFER`, and half the window, so a single reply can't crowd the rest of the history out of the next compaction. It's never below `MIN_REPLY_TOKENS` (16,384, the old fixed value) or half a smaller window; the compaction trigger keeps that much room, so a request that would leave less compacts first, and the budget is then computed on the smaller history. The output cap is the model's "Max reply tokens", else what its provider reported (`provider_models.reported_max_output`). The request's input is the last real usage plus an estimate of what's new (`projected_input`), or, with nothing measured since the history changed shape (a first request, or one just after a compaction), an estimate of the whole request (`estimate_request_tokens`). A model on an Anthropic or Other provider that nothing sized is capped at 16,384 (`providers::UNKNOWN_OUTPUT_CAP`): a Claude model refuses a `max_tokens` above its own cap. A reply that stops with `stop_reason: "max_tokens"`, or `"model_context_window_exceeded"` (Claude 4.5+, the window reached first; the notice then gives the tokens it wrote), is kept, and the turn saves a notice after it (`api::chat::cut_off_notice`, a user-role message like the stop notice, so the model knows its reply is incomplete): every tab shows it live as the transcript's notice line, naming the limit, what bound it (`ReplyLimit`: the model's cap, which "Max reply tokens" can raise; an Anthropic model's own reported cap, which it can't; half the window; or the room left) and what to change. It isn't continued automatically: Claude refuses a prefilled partial reply, and a local model would start its reasoning over.

**A reply cut off in the middle of a tool call** (SME-126): every provider stops each content block before the `message_delta` that carries `stop_reason`, so `anthropic::stream` keeps a finished call whose input doesn't parse until the stream ends. When the reply was cut off (`stream::is_cut_off`) and its last block is a tool call, that call is dropped, whether or not its input parsed, and `StreamedTurn::cut_off_call` names it, when its name is a plain tool name (the notice is a user-role message the model reads; anything else, or none, is dropped unnamed); the notice says which call it was. It's never run or saved. A cut-off reply's whole calls before it are saved but not run (its `stop_reason` isn't `tool_use`), each with a saved error result (`turn::CUT_OFF_TOOL_CALL`). A reply that was nothing but the cut call saves no assistant message, only the notice. Any other unparseable input (a reply that wasn't cut off, or a call that isn't the last block) still fails the turn.
- **No model yet** (a new conversation, one from before providers, or one whose provider was deleted): it takes the default (`db::adopt_default_model`, one statement, which leaves a model the picker set meanwhile alone), keeps it, and publishes `ModelChanged`.
- **No default either:** the turn fails with `providers::NO_MODEL_CONFIGURED`. The chat page shows that with a link to `/providers` rather than as an error, for a `TurnError` and a `NotificationDeliveryFailed` alike; the picker disables Send until there's a model.
- **After a switch** of backend, thinking blocks written before it aren't replayed: their signatures were issued for the other one. The switch is recorded when a turn *starts* on a backend (provider, base URL, model) other than the last turn's (`db::record_turn_model`, under the turn lock), not when the model is picked, since a turn still running keeps writing for the old one; editing a provider's base URL counts too. `history_for_request` drops thinking up to `conversations.model_changed_at_message_id`; a message that was only thinking keeps its reasoning as plain text (the API rejects empty content). A conversation from before providers counts as switched on its first turn, since it may have run on another backend. Nothing stored changes.
- **The context indicator and detail view** show the conversation's model's window, or the default's, without saving anything (`providers::conversation_context_window`).

If a request fails with Ollama's specific "error parsing tool call" 500 (its Anthropic-compat shim, at least for `gpt-oss` models, doesn't always turn a model's raw output into valid tool-call JSON — either because thinking's reasoning landed in the same text as the call, or the model just wrote invalid JSON on its own), `run_turn_bounded` retries: first with `thinking` dropped, then up to `TOOL_CALL_PARSE_RETRIES` further plain regenerations, since a local model's next sampling pass often doesn't repeat the same malformed output. Gives up and surfaces the error once that's exhausted. Any other failure propagates from `run_turn_bounded` immediately. See `is_ollama_thinking_tool_call_corruption`.

One layer down, `anthropic::stream::stream_anthropic_message` retries a briefly unavailable provider on its own: an HTTP 429, 503 or 529 gets two more attempts, 1s and then 3s later (`RETRY_DELAYS`), before anything has streamed. Any other status fails at once. A failure reads `model provider error {status}: {message}`, where the message is the provider's own (`provider_error_message` unwraps the JSON error body, including a nested one), not the raw response.

`run_turn_body` refuses a conversation that doesn't exist with a plain `conversation not found` (checked right after taking the conversation's lock), rather than letting the insert fail on a foreign key; `start_turn` makes the same check before spawning, so `send_message` returns the error itself. `get_messages` returns the same error, which the chat page uses to show "This conversation doesn't exist" instead of a message box. The new message is saved *before* the credential check, so with no credentials configured the message is kept and only the reply fails.

**Some rows are not persisted by the request that "sent" them.** A finished terminal command (`Notice::Wake`) and a notice from outside a turn (`Notice::Deliver`, e.g. Docker restarting or a trust decision), both sent through `turn::notify`, push their own turns onto the conversation via `run_turn`, with no `send_message` call in flight at all. Those rows are stored exactly like any other turn (see [models.md](models.md)), just written outside the request/response cycle a naive reading of this API might assume. See "Live conversation events" below for how a browser tab learns about them.

## Live conversation events

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
// src/events.rs
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
    TurnState { running: bool },
    ReplyReset {},
    ReplyDelta { text: String, offset: usize },
    TurnError { message: String },
}
```

`MessagesAppended` is a struct variant, not `MessagesAppended(Vec<Message>)`: the enum is internally tagged, and serde can't write a tuple variant holding a list that way. It used to be one, and every send failed silently (`wire_tests::test_every_event_round_trips_through_json` now serializes one of each variant). The stream is built with `ServerEvents::from_stream` over the broadcast receiver, for the same reason as the frame stream below: a closed connection drops the subscription. A subscriber that falls so far behind that the channel drops events for it (`RecvError::Lagged`) has its stream ended, so the tab reconnects and pulls the current state rather than missing a saved message or a turn's end. `TurnError` and `NotificationDeliveryFailed` split failures by who started the turn: a turn the user started by sending reports as `TurnError`; one started with no request in flight (a finished terminal command's `Notice::Wake`, a `Notice::Deliver`) reports as `NotificationDeliveryFailed`. A stop is neither's failure and isn't reported this way.

`SandboxCommandUpdate` with `command: Some` is a command starting: `sandbox::run_command` publishes it before sending the command to the pod's agent, so a fast command's finish, which comes back on the pod's reader task, can't reach a tab first. With `command: None` it's an output line (`stream` and `latest_output` set), a finish (`finished` and the exit code), or `lost` (`run_command`'s send failed; no exit code). The page applies each to the command with that `command_id`, which needn't be the terminal's newest (SME-144).

`Sandbox*`/`ContextUsageUpdate`/`TodoListUpdate`/`BrowsingSessionUpdate`/`BrowsingUrlUpdate` are all ephemeral UI telemetry (never persisted as such, regenerable at any time from `get_sandbox_state`/`get_context_usage`/`get_todos`/`get_browsing_state` — `ContextUsageUpdate`'s own numbers *are* separately persisted, in `conversation_context_usage`, precisely so `get_context_usage` can regenerate it, and `TodoListUpdate`'s are likewise persisted in `conversation_todos`); `MessagesAppended` is a live-delivery notification for rows `run_turn` already persisted — whether that `run_turn` call came from a live `send_message` or from a notice that woke the model. `SandboxPodUpdate`/`SandboxTerminalUpdate` fire on create/terminate (a `terminated: true` update means the frontend should *remove* that pod/terminal, not just relabel it); `SandboxCommandUpdate` is one variant for "started", "one output line" and "finished", with `command` only populated on the "started" event. It carries `position` for an output line (the agent's per-command `seq`), so a tab that reconnects skips lines its snapshot already has. `ContextUsageUpdate` publishes once per completed real turn, right after `run_turn_bounded` persists that turn's usage. `TodoListUpdate` publishes on every `todowrite` call and always carries the *complete* current list (never a partial diff — `todowrite` itself is a whole-list replace, no per-item ids), so the frontend just overwrites its signal wholesale rather than merging line by line. `BrowsingSessionUpdate` publishes from `open_browser_session`/`close_browser_session`, so the panel shows/hides reactively rather than only checking on conversation (re)select. `SandboxPreviewUpdate` publishes from `sandbox_preview_url` and carries the pod's whole preview list (port and link), which the panel takes as is; `get_sandbox_state` returns the same list per pod (SME-42). Since the underlying `broadcast` channel has no replay, the frontend does a one-shot reconciliation pull on connect/reconnect to cover anything published before it subscribed: `get_messages`, `get_sandbox_state`, `get_context_usage`, `get_todos`, `get_pending_question`, `list_conversation_repos`, `get_turn_state`, `get_reply_in_progress`, `get_turn_error` and `get_browsing_state` — see [architecture.md](architecture.md).

`ReposUpdate` carries the conversation's whole repo list (SME-32): each repo's path, what's checked out, clone status, its `AGENTS.md` files, which of them are loaded, and any loads waiting on the user's trust (with the file shown on the card). It's published when a repo is added, starts or finishes cloning, when the model loads or asks to load an `AGENTS.md`, and when a trust decision is made. `list_conversation_repos` is its snapshot.

`ModelChanged` says the conversation's provider or model changed (the picker, or a turn taking the default); the picker refetches `get_conversation_model`. `PodsChanged`, `TurnsChanged` and `ProvidersChanged` are the app-wide `AppEvent`s (below), relayed on every conversation's stream: the server merges the app-wide channel into each subscription (`conversation_event_stream`). A chat tab therefore needs no second always-open connection for the sidebar's pod dots and busy marks: on each, the sidebar refetches `get_live_pod_conversations` or `get_busy_conversations`. Over plain HTTP/1.1 a browser allows only 6 connections per host, shared by every tab, and a chat tab holds one stream (`subscribe_conversation_events`), plus `subscribe_browser_frames` while a browsing session is open. `TurnState` is published when a conversation's first turn starts and when its last one ends, including turns nobody started from a tab (a finished command, a clone or other notice, another tab). `get_turn_state` is its snapshot for reconnects.

### Stopping a turn

`stop_turn(id)` ends the conversation's running turn, and any turn queued behind it:
- **How:** each conversation has a stop counter (`TurnStops`, a `watch` channel, in its `turn::state` entry). `run_turn_bounded` notes the counter's value, waits for the turn lock in a `select!` against it changing, saves the turn's own message holding the lock and outside any `select!` (so a stop can't land between the save and the turn knowing it's saved; SME-91), then runs the turn (`run_turn_body`) in a `select!` against the counter. A stop drops the turn wherever it's waiting (the model's stream, a tool call, a compaction), which releases the lock. Turns that start after the stop aren't affected.
- **Left alone:** the pod, its terminals and running commands.
- **What's saved:** a `STOP_NOTICE` user message ("The user stopped this turn before it finished…"), once however many turns the stop ended (`record_stop`, which records the last stop it noted), so the model doesn't take the request as still waiting for an answer; the chat page shows it as "Stopped.". A queued turn's own message that wasn't saved yet (a finished command's notice, say) is saved anyway, without running a turn for it. Each ended turn fails with `TURN_STOPPED`, which a user's turn reports as `TurnError` (the page shows "Stopped.", and it isn't kept for `get_turn_error`).
- **What's left behind:** a reply that was streaming isn't saved; the stop clears it (`ReplyReset`), also while another turn is still queued. Tool calls the model had made but that hadn't returned have no result. `answer_unfinished_tool_calls` fixes that when each request is built: every tool call without a result gets an error result (`UNFINISHED_TOOL_CALL`), at the start of the next user message, or in a new one at the end. It isn't saved, since notices can already follow the dangling call. This also repairs a turn cut off by a server restart.
- **The pause:** a stop also pauses the conversation (`ConversationRuntime::paused`, in memory). While paused, `Notice::Wake` does nothing (a finished command's notice stays pending, and the next turn drains it), and a `Notice::Deliver` is only saved, as a `Notice::Save` is, instead of starting a turn. `send_message` ends the pause. Without it, a command still running would restart the model seconds after the user stopped it.

Everything outside a turn reaches the model through one function, `turn::notify(pool, id, notices)` (SME-52). It runs its notices in order in a task of its own and returns at once, since each waits for a running turn and the turn lock isn't re-entrant (a caller inside a turn could otherwise deadlock). `Notice::Save(text)` saves a `user` notice only while no turn holds the conversation's lock, so it can't land between a tool call and its result (an order the API rejects): the user stopping a pod, and the pod-crash notice. `Notice::Deliver(text)` also runs a turn for it, unless paused: Docker restarting, a language server stopping, a trust decision. `Notice::Wake` drains finished commands' notices and runs a turn for them, if any: a command finishing, and after the pod-crash notice. A failed delivery or wake is published as `NotificationDeliveryFailed`.

### The model's questions (`ask_user`, SME-34)

The model can ask the user one to four questions (`ask_user`: each a question, a header up to 30 characters, and two to four options, single or multi-select, or none for free text). The turn loop handles the call itself (`turn::questions`), not the tool registry:
- **Asking:** the other tools in that reply run as usual and their results are saved; a second `ask_user` in the same reply, or input over the limits, is an error result. Then a `pending_questions` row records the question (`answer` null), `QuestionUpdate { question: Some(..) }` and `AppEvent::QuestionsChanged` go out, and the turn ends with the call unanswered.
- **Waiting:** a turn that finds a question waiting saves what it brought (a notice, a finished command) and returns before calling the model (the check is in `run_turn_body`, after the drain), so every wake path waits too.
- **Answering:** `answer_question` checks the answer against the question and writes it onto the row with `UPDATE … WHERE answer IS NULL` (one tab wins; another gets "This question was already answered."), ends a Stop's pause and starts a turn. Whichever turn next holds the conversation's turn lock takes the row and puts the call's result at the front of its message, in one transaction with saving that message (`db::create_message_taking_question`), so a notice's turn that gets there first carries the answer instead. The result is `The user answered:` and one line per question (`1. <header>: <labels>[; they also wrote: "<text>"]`).
- **Writing instead:** a message the user sends while a question waits takes it (`from_user` on `run_turn_bounded`, set only by `start_turn`): the result is "The user didn't answer these questions. Their message follows.", then their text.
- **History:** notices saved while waiting sit between the call and its result, which the API rejects; `answer_unfinished_tool_calls` moves a result found in a later user message of the same run to the front of the message right after the call (`move_late_results`). Storage keeps the order things happened in.

### App-wide events and pods

`subscribe_app_events` is the stream of `AppEvent` (`src/events.rs`), for views that span conversations. Both events carry nothing; listeners refetch:
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

### The browsing panel's own live channel

Unlike every other panel above, the browsing panel's live frames are **not** part of `ConversationEvent` — they're frequent, ephemeral, UI-only data (a base64 JPEG per screen update) that shouldn't crowd out task/message updates in that shared bounded broadcast channel, so they get a dedicated per-conversation channel instead, in `src/browsing.rs`, with its server functions in `api::browsing`:

```rust
#[get("/api/conversations/{id}/browsing/frames")]
pub async fn subscribe_browser_frames(id: i64) -> ServerFnResult<ServerEvents<browsing::BrowserFrame>>;

#[post("/api/conversations/{id}/browsing/input")]
pub async fn send_browser_input(id: i64, event: browsing::BrowserInputEvent) -> ServerFnResult<()>;

#[post("/api/conversations/{id}/browsing/navigate")]
pub async fn navigate_browser(id: i64, address: String) -> ServerFnResult<()>;
```

`navigate_browser` is the address bar's endpoint. A bare host gets `https://` (`browsing::normalize_address`); then it runs the same `browsing::navigate` the model's `browser_navigate` uses, so the scheme check and SSRF guard apply identically. The address bar learns the resulting URL the same way it learns about every other navigation: `BrowsingUrlUpdate`, published whenever the session page's main-frame URL changes (`frameNavigated`, plus `navigatedWithinDocument` for `pushState`/fragment changes). A failed load reports the address that failed, not Chrome's `chrome-error://` page.

`subscribe_browser_frames` starts the real CDP screencast (`Page.startScreencast`) the moment the *first* viewer subscribes and stops it (`Page.stopScreencast`) when the *last* one disconnects — reference-counted, via a `Drop`-based guard inside `browsing::FrameSubscription`. The count only flips a flag; one long-lived task per session (`browsing::run_screencast`) issues every start/stop command itself, so a stop meant for a departed viewer can never land after a new viewer's start. Like every stream here (`subscribe_conversation_events`, `subscribe_app_events`), it's built with `ServerEvents::from_stream`, not `ServerEvents::new`, and that choice matters: `new` runs its closure as a detached task feeding an unbounded queue, so a closed connection is never noticed (the viewer would never be released) and a slow reader would build an unbounded backlog of frames. With `from_stream` the response body *pulls* each frame only when the connection can take it: a slow viewer just gets the newest frame on its next pull, and a closed connection drops the stream and the viewer with it. Frames travel over a `watch` channel holding only the latest frame, not a broadcast channel: Chrome only sends a frame when something on screen changes, so a viewer joining a running screencast on a still page (a second tab, a reload) would otherwise wait forever. With `watch`, a new viewer gets the current frame at once. `send_browser_input` forwards one mouse/keyboard event (in the frame's own fixed pixel space — the session's viewport is pinned to match the screencast bounds exactly, so no scale-factor lookup is needed) to the real page via `Input.dispatchMouseEvent`/`dispatchKeyEvent`/`insertText`. The panel sends these one request at a time, in order (see [frontend.md](frontend.md#the-live-browsing-panel)), since separately issued requests can overtake each other. There's no locking between this and the model's own tool-driven `browser_click`/`browser_fill`/... calls — both act on the same real page, and CDP just serializes whichever commands arrive.

## Current endpoints

| Function | Method + path | Notes |
|---|---|---|
| `get_conversations` | `GET /api/conversations` | ordered by `updated_at DESC` |
| `create_conversation` | `POST /api/conversations` | default title |
| `get_messages` | `GET /api/conversations/{id}/messages` | ordered by `created_at ASC` |
| `send_message` | `POST /api/conversations/{id}/messages` | starts the user's turn and returns; the reply arrives on the conversation's event stream, see above |
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
| `get_turn_error` | `GET /api/conversations/{id}/turn-error` | the last failed turn's error, until the user writes again, for a tab that connects after `TurnError` |
| `get_busy_conversations` | `GET /api/conversations/busy` | conversations with a turn running or queued, for the sidebar |
| `get_conversation_model` / `set_conversation_model` | `GET`, `POST /api/conversations/{id}/model` | the picker: which model the next turn uses (`ConversationModel`: `Chosen`, `Default`, `NoDefault`, `NoProviders`), and choosing one (SME-72) |
| `list_providers` / `get_provider` | `GET /api/providers`, `GET /api/providers/{id}` | the `/providers` pages; never the secret, only `secret_hint` |
| `create_provider` / `update_provider` | `POST /api/providers`, `POST /api/providers/{id}` | checked (a name, an http(s) base URL, a key when new); an empty key on update keeps the stored one, unless the base URL changed: the key only goes where it was entered for |
| `delete_provider` | `DELETE /api/providers/{id}` | conversations using it, and the default if it was the default's, go back to having none |
| `list_provider_models` | `GET /api/providers/{id}/models?ask_each` | asks the provider (see [setup.md](setup.md#model-providers)), stores what it reports, and adds models with stored settings it didn't list; a listing error is returned with them. `ask_each` also asks an Ollama server about every model (the provider's page); the picker leaves it off |
| `probe_llama_cpp` | `POST /api/providers/probe-llama-cpp` | whether an address answers `/props` like llama.cpp, for the provider form's suggestion (SME-111); asked without any key |
| `add_provider_model` | `POST /api/providers/{id}/models/add` | a model the listing doesn't show, keeping any settings it has |
| `refresh_model_details` | `POST /api/providers/{id}/models/refresh` | asks about one model, when it's chosen |
| `set_model_settings` | `POST /api/providers/{id}/models/settings` | a model's settings (`ModelSettings`): thinking, context window, effort and max reply tokens (SME-111) |
| `get_default_model` / `set_default_model` | `GET`, `POST /api/default-model` | the default provider and model |
| `get_pods` | `GET /api/pods` | every live pod with status, activity, limits and usage, see above |
| `stop_pod` | `POST /api/pods/{pod_id}/stop` | the user stopping a pod, see above |
| `get_live_pod_conversations` | `GET /api/pods/conversations` | conversations with a live pod, for the sidebar |
| `subscribe_app_events` | `GET /api/app-events` | always-open app-wide stream (`AppEvent`), for the pods page |
| `get_build_id` (`api::version`) | `GET /api/build-id` | the server's `SMELT_BUILD_ID`; a tab asks on every stream (re)connect and offers a reload when it isn't its own (see [architecture.md](architecture.md#live-channels)). The browser tier can override it (`test_override`, `browser-test` only) |
| `list_conversation_repos` | `GET /api/conversations/{id}/repos` | the conversation's repos, for the sandbox panel, see `ReposUpdate` above |
| `attach_repo` | `POST /api/conversations/{id}/repos` | "Work on a repo": trusts the remote, starts the sandbox if needed and clones |
| `decide_repo_trust` | `POST /api/conversations/{id}/instruction-requests/{request_id}` | the trust card's answer; remembered for the remote; on Trust loads exactly the file the card showed; tells and wakes the model |
| `get_pending_question` (`api::questions`) | `GET /api/conversations/{id}/question` | the question the conversation waits on (none once answered), for the card; live as `QuestionUpdate` |
| `answer_question` | `POST /api/conversations/{id}/question/answer` | the card's answer, one per question; recorded once, then a turn sends it (see "The model's questions") |
| `get_waiting_conversations` | `GET /api/questions/waiting` | conversations waiting on an answer, for the sidebar |
| `get_git_settings` | `GET /api/git` | the `/git` page: commit identity, SSH keys (public halves), trust decisions |
| `save_git_identity` | `POST /api/git/identity` | commit name and email; reaches running pods at once |
| `generate_ssh_key` / `import_ssh_key` | `POST /api/git/keys`, `POST /api/git/keys/import` | a new ed25519 key, or a pasted unencrypted OpenSSH private key; installed in running pods at once |
| `delete_ssh_key` | `DELETE /api/git/keys/{id}` | removed from running pods at once |
| `forget_repo_trust` | `POST /api/git/trust/forget` | the user is asked again next time the remote is cloned |
| `list_language_servers` / `get_language_server` | `GET /api/language-servers`, `GET /api/language-servers/{id}` | the Language servers page (SME-35) |
| `create_language_server` / `update_language_server` | `POST /api/language-servers`, `POST /api/language-servers/{id}` | validated; a rename or disable stops the server's pods |
| `delete_language_server` | `DELETE /api/language-servers/{id}` | stops its pods everywhere |
| `lookup_language_server` | `GET /api/language-servers/lookup/{package}` | a suggested config from mason's registry and Helix |
| `list_mcp_servers` / `get_mcp_server` | `GET /api/mcp-servers`, `GET /api/mcp-servers/{id}` | the `/mcp-servers` pages; header names only, never values (see [mcp.md](mcp.md)) |
| `mcp_server_status` | `GET /api/mcp-servers/{id}/status` | a real connection attempt: `Connected { tool_names }`, `Unreachable { error }`, `NotConnected` or `NeedsReconnect { error }` (an OAuth sign-in that can't be refreshed, SME-113) |
| `create_mcp_server` / `update_mcp_server` | `POST /api/mcp-servers`, `POST /api/mcp-servers/{id}` | update merges headers and drops the cached connection |
| `delete_mcp_server` | `DELETE /api/mcp-servers/{id}` | also drops the cached connection |
| `start_mcp_server_oauth` | `POST /api/mcp-servers/{id}/oauth/start` | returns the authorization URL the browser goes to (SME-16) |
| `disconnect_mcp_server_oauth` | `POST /api/mcp-servers/{id}/oauth/disconnect` | clears the stored OAuth credentials |
| `list_sandbox_volumes` / `create_sandbox_volume` | `GET /api/sandbox-volumes`, `POST /api/sandbox-volumes` | volumes mounted into sandbox pods; create makes the PVC and expands a leading `~` in the mount path |
| `delete_sandbox_volume` | `DELETE /api/sandbox-volumes/{id}` | deletes the PVC, then the row |
| `delete_conversation` | `DELETE /api/conversations/{id}` | hard delete; cascades to the conversation's messages (`ON DELETE CASCADE`); also tears down its sandboxes and browsing session, and drops its event channel and turn lock; deleting a nonexistent id is not an error |

Not yet implemented (a straightforward addition when needed): renaming a conversation.
