# API — Dioxus Server Functions

There is no hand-rolled Axum API layer and no hand-written browser fetch client. Every endpoint is a plain async function in one of `src/api/`'s modules (`chat`, `browsing`, `git`, `language_servers`, `mcp`, `pods`, `sandbox_volumes`, `secrets`) decorated with `#[get(...)]`, `#[post(...)]` or `#[delete(...)]` (from `dioxus::prelude`, available under the `fullstack` feature). The macro generates two different bodies for the same function signature:

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
- **`GET /oauth/mcp-callback/{id}`** → `mcp_oauth::callback_handler`, a plain Axum route. The OAuth provider sends the user's browser here, and it has to answer with a real redirect back into the app, not JSON: `/mcp-servers/{id}`, or `/mcp-servers/{id}?oauth_callback=1` after a failure, whose error stays on the server. See [mcp.md](mcp.md#oauth).
- **`request_guard::guard`**, a layer over every route (SME-51). smelt has no login, so it refuses what another site's page could send on the user's behalf. A request that isn't `GET`/`HEAD`/`OPTIONS` is refused when `Sec-Fetch-Site` says it came from another site (`same-site` included: a sandbox preview is a sibling origin), or, without that header, when `Origin` names another host. Once `SMELT_ALLOWED_HOSTS` is set, a `Host` that isn't one of those names or `SMELT_BASE_URL`'s host is refused too (IP addresses and `localhost` always pass). A request with neither header (curl, a script) is let through.

The sandbox preview proxy (`src/preview.rs`, SME-42) isn't on this router at all: it has a listener of its own, `SMELT_PREVIEW_ADDR` (default `0.0.0.0:8181`), because the preview's address is all in `Host` and `dx serve`'s dev proxy replaces `Host`. See [setup.md](setup.md).

## Sending a message: `send_message`

`send_message(id, content) -> ServerFnResult<()>` is an ordinary request (`start_turn` in `api::chat`):
1. It checks the conversation exists (`conversation not found` otherwise).
2. It admits the turn (`TurnInFlight::start_by_user`: ends any pause from an earlier stop and counts the turn as in flight, so a Stop sent after `send_message` returns ends it; SME-216), starts it in a background task and returns, without waiting for the turn.

Turns in one conversation don't overlap: each takes the conversation's lock (`conversation_lock`) and queues behind whatever turn is running, whether a second send or a notice (a finished command, a clone, a Docker restart).

The turn is `turn::run_turn_bounded`, a *loop* (capped at `MAX_TURNS`), not one Anthropic call: whenever the model's `stop_reason` is `"tool_use"`, it runs each tool (`anthropic::tools::execute`), saves the `ToolResult` turn, and loops again. Everything it does reaches the browser on the **conversation's event stream** (`subscribe_conversation_events`, below), in every tab watching the conversation, not only the one that sent:
- **`MessagesAppended`** for each message as it's saved: the user's own message first (which replaces the sender's optimistic copy), then each tool call, tool result and reply.
- **`ReplyReset {}`** when each model call starts (and again on each retry, whose text replaces the failed attempt's), and **`ReplyDelta { text, offset }`** as its text streams. So each model call is its own streaming bubble, and a multi-step reply no longer runs together. `offset` is the reply's length in bytes before `text`, so a tab that has just fetched the reply so far skips what it already has (SME-51).
- **`TurnState { running, elapsed }`** when the turn starts and ends (published in the same step that counts the turn in or out, so a turn starting as the last one ends is always told last; SME-179): `elapsed` is how long the busy spell had been running, in whole seconds, on the server's monotonic clock when the event was published (0 at both transitions — SME-160).
- **`TurnError { message }`** if the user's turn fails or is stopped (`TURN_STOPPED`). The message is the error's own text (`chat_error_text`), without `ServerFnError`'s "error running server function: … (details: None)" wrapper. A failure (not a stop) is also kept in memory (the conversation's `turn::state` entry) until the user writes again, and `get_turn_error` serves it to a tab that connects afterwards. It's kept only if no newer turn has started since the failed one did (a turn generation per conversation, bumped by the user sending or answering a question, by a turn with a message of its own once it holds the conversation's lock, and by a wake or an answer's turn only once it has something for the model: the answer, or notices), so a turn that fails after the user already sent the next message doesn't leave its error under that message's reply (SME-91), and a wake or answer's turn with nothing to do, queued behind the failing turn or coming after it, leaves the error alone (SME-179). A wake with nothing pending isn't even counted, so tabs aren't told a turn started and keep showing the error too (SME-218). A failed turn also clears its reply in progress (`ReplyReset`) while it still holds the lock, as a stopped one does, so a turn queued behind it doesn't show the failed text (SME-179). A "Work on a repo" that couldn't start the sandbox for a repo already checked out is shown the same way (`show_conversation_error`).

**A tool result is at most `MAX_TOOL_RESULT_CHARS` (30,000) characters**, for every tool, MCP or native, success or error: `execute` cuts anything longer and appends a line saying how much was cut and to narrow the request (SME-76). The rest is dropped, not saved anywhere. `read_file` and `read_terminal_output` stay under it on their own (`fit_lines`): a line over 2,000 characters is cut, and a read stops at a whole line with `truncated: true` and the `next_offset` to read from, instead of being cut mid-line by `execute`.

`read_file`'s cut line ends with a note naming the line, how long it is and the `offset` and `character` to read on with — the only way into a long line, and the one thing `read_terminal_output`'s cut line still doesn't give (SME-189). `read_file` also takes `character` (1-based, counted in Unicode scalar values, exactly as `lsp` counts it): the character of the `offset` line to start the read at, so a 40,000-character line takes twenty reads instead of dying at 2,000. Such a read returns that line alone — the page holds no other line — so walking a long line costs the line rather than everything after it (SME-189 review 1), and it says `"character"` in its JSON, so the model knows the text it got isn't the line's start; nothing is added inside `content`, so text copied out of a read stays real file text. `next_offset` stays the next *whole* line, and `truncated` stays "lines left out for the budget". The notes are appended after `fit_lines`, which is why they cost nothing against `LINES_BUDGET_CHARS`: at 2,012 characters apiece (`fit_lines` sizes the JSON encoding, quotes included), at most 12 cut lines fit a read, and their notes fit in the 5,000 characters between the budget and the cap.

**Why not stream the reply on the request itself**, as it used to (`ServerEvents<ChatEvent>`)? Because that held a second connection open per tab for the whole reply. Over plain HTTP/1.1 a browser allows 6 connections per host across all tabs, so with a few tabs open a new tab couldn't load and a click (Stop) waited for the reply to end (see [SME-27](https://linear.app/smelt-agent/issue/SME-27)). Now a tab holds one connection, plus the browsing panel's while that's open. It also means every tab sees a reply live, and a tab that (re)connects mid-reply shows the text so far (`get_reply_in_progress`, from the conversation's in-memory `turn::state` entry, cleared when a call starts, when its reply is saved, and when the turn ends). A delta is added there and published under the same lock `get_reply_in_progress` reads through (`relay_reply_delta`), so the fetched text and the offsets that follow agree.

Every turn request carries a **system prompt**: `system_prompt(&prompt_environment(pool, conversation_id).await)` in `api::chat`. It's per conversation, and has three parts:
- **The fixed base prompt**, `src/turn/system_prompt.md`, compiled in with `include_str!`. After a line on smelt as a coding agent, its sections are: Tools and their owners (which since SME-309 says up front that MCP descriptions carry their server's label and that an MCP comment tool is a ticket record, not a message between agents), Your sandbox, Running commands, Files, Language servers, Docker, Git, The web, Servers you run, Working style, and How your replies are shown (plain text, not rendered markdown).
- **An environment section** built from the database and clock for each request: today's date (UTC), the model, the configured volumes with their mount paths, the configured MCP servers — under a lead line giving the `mcp__<server>__<tool>` rule, one line per server, each with the purpose its `initialize` handshake stated once a connection has captured it (SME-309; [mcp.md](mcp.md#tools)) — and the conversation's repositories (path, URL, and the branch, "still cloning" or "clone failed", with its `AGENTS.md` files, the loaded ones marked `(loaded)`). Every offered MCP tool's description starts `[server] `, composed at offer time (`mcp::labelled_description`), so ownership is legible at the choice point. A failed database read leaves its part out and is logged, rather than failing the turn.
- **The loaded project instructions**: each `AGENTS.md` the model has loaded, in full (`git::render_project_instructions`).

The prompt changes with the date and configuration, and within a conversation when its repos or loaded instructions change. `test_system_prompt_only_names_real_tools` fails if the base prompt names a tool that doesn't exist, so renaming a tool means updating the prompt too. Compaction's summarization call keeps its own `COMPACTION_SYSTEM_PROMPT`.

Requests carry `thinking: {"type": "adaptive"}` unless the conversation's model has it off (its setting on `/providers`, or its Ollama server saying it can't think — see [setup.md](setup.md#model-providers)), so an assistant turn's `content` can start with a `ContentBlock::Thinking { thinking, signature }` block ahead of any `Text`/`ToolUse` blocks — `run_turn_bounded` persists and replays it exactly like any other block, uninterpreted; the frontend renders it as a collapsed-by-default `<details>` (see `frontend/pages/chat/transcript.rs`'s `render_block_element`).

**Content blocks are accumulated by their `index`** (SME-112). Anthropic and Ollama close each block before starting the next, but llama.cpp's `llama-server` opens thinking, text and each tool call without closing any, and sends every `content_block_stop` at the end of the reply. `stream_anthropic_message` keeps every open block (`Blocks`), and the reply's content is in index order, whatever order the stops come in; a block still open when the stream ends is kept. Before this, a tool call's start replaced the text before it, so llama.cpp's commentary between tool calls showed while streaming and was gone once the message saved. A `content_block_start` for `text` or `thinking` still opens nothing (its first delta does), so a thinking block Anthropic sends with its text omitted is still dropped. llama.cpp's thinking blocks are now saved with an empty signature and replayed.

**A provider that refuses replayed thinking** (SME-93). Under Anthropic's preserved thinking a thinking block is bound to the `system` prompt, the `tools` and every message before it, and smelt's system prompt changes within a conversation (above), as does its tool list (MCP servers). Accounts created on or after 2026-08-31 refuse a request that replays a block whose prefix changed, with a 400 saying it's "bound to a different conversation". `stream_anthropic_message` retries such a request once more before anything has streamed: with thinking on, under the `thinking-binding-controls-2026-08-01` beta with `thinking.block_binding.prefix_mismatch_behavior: "drop_block"` (the API drops what it can't accept); with thinking off, or if that retry is refused too, with the history's thinking blocks removed. Which form worked is remembered per provider (base URL and credential, in memory; enforcement is per account), so later requests go out that way without the extra 400; a restart forgets it. A provider that never sends that 400 (older accounts, Ollama, other servers) never gets the beta or the field: setting the field would opt an older account into the check. Drops the API reports in `message_start`'s `input_transformations` are logged. The earlier reasoning is lost either way; keeping it means sending prompt and tool changes append-only (SME-98).

### Which model a turn runs on

In [api/model.md](api/model.md).

## Live conversation events

In [api/events.md](api/events.md).

## Child agents

`start_agent`, `list_agents`, `stop_agent` and `finish`, the start, the queue that carries reports and notices, and the subtree delete (SME-236) are in [api/agents.md](api/agents.md).

### Stopping a turn

In [api/events.md](api/events.md#stopping-a-turn).

### The model's questions (`ask_user`, SME-34)

In [api/events.md](api/events.md#the-models-questions-ask_user-sme-34).

### App-wide events and pods

In [api/events.md](api/events.md#app-wide-events-and-pods).

### The browsing panel's own live channel

In [api/events.md](api/events.md#the-browsing-panels-own-live-channel).

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
| `stop_turn` | `POST /api/conversations/{id}/stop` | the Stop button: end the running turn and pause, see above; refused on a child agent still starting, and stopping a working one tells its parent (SME-236) |
| `get_turn_state` | `GET /api/conversations/{id}/turn` | whether a turn is running, with how many whole seconds its busy spell has run, for the Stop button and the "Working…" timer on (re)connect (SME-160) |
| `get_reply_in_progress` | `GET /api/conversations/{id}/reply` | the reply streamed so far, for a tab that (re)connects mid-reply |
| `get_turn_error` | `GET /api/conversations/{id}/turn-error` | the last failed turn's error, until the user writes again, for a tab that connects after `TurnError` |
| `get_busy_conversations` | `GET /api/conversations/busy` | conversations with a turn running or queued (the sidebar reads `get_conversation_tree` since SME-246) |
| `get_conversation_tree` (`api::agents`) | `GET /api/conversations/tree` | the sidebar's list (SME-246): every conversation, most recently active first, with its `parent_id`, its busy and waiting marks, and for an agent its persona (`<dir>:<name>`, from the stored persona path) and status, derived at each call by `agents::status` as `list_agents` gives it, never stored. Refetched on `ConversationsChanged`, `TurnsChanged` and `QuestionsChanged`; the client lays it out as a tree |
| `get_agents_waiting` (`api::agents`) | `GET /api/conversations/{id}/agents-waiting` | the agents under a conversation, at any depth, waiting for the user (an `ask_user` question or a trust card, never a question to their parent), for the banner over its chat (SME-246) |
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
| `subscribe_app_events` | `GET /api/app-events` | always-open app-wide stream (`AppEvent`), for the pods page and for a chat tab with no conversation open (SME-269) |
| `get_build_id` (`api::version`) | `GET /api/build-id` | the server's `SMELT_BUILD_ID`; a tab asks on every stream (re)connect and offers a reload when it isn't its own (see [architecture.md](architecture.md#live-channels)). The browser tier can override it (`test_override`, `browser-test` only) |
| `list_conversation_repos` | `GET /api/conversations/{id}/repos` | the conversation's repos, for the sandbox panel, see `ReposUpdate` above |
| `attach_repo` | `POST /api/conversations/{id}/repos` | "Work on a repo": trusts the remote (under its trust lock, dropping its cards, SME-224), starts the sandbox if needed and clones; runs on if the tab closes, and a failure no request read is shown in the conversation (SME-225; "read" by the server function: the tab keeps one it read for its conversation through a switch, SME-263, but a response lost after the read shows nowhere, SME-224 review 2); one failing after the trust committed still tells the models whose cards it dropped, and one from a deleted conversation trusts nothing (SME-224) |
| `decide_repo_trust` | `POST /api/conversations/{id}/instruction-requests/{request_id}` | the trust card's answer; remembered for the remote; on Trust loads exactly the file the card showed; tells and wakes the model; runs on if the tab closes (SME-225) |
| `get_pending_question` (`api::questions`) | `GET /api/conversations/{id}/question` | the question the conversation waits on (none once answered), for the card; live as `QuestionUpdate` |
| `answer_question` | `POST /api/conversations/{id}/question/answer` | the card's answer, one per question; recorded once, then a turn sends it (see "The model's questions") |
| `get_waiting_conversations` | `GET /api/questions/waiting` | conversations waiting for the user: an unanswered `ask_user` question or, since SME-246, an AGENTS.md trust card (`db::WAITS_FOR_USER`, the one predicate the tree and `list_agents` use too). The sidebar reads `get_conversation_tree` since SME-246 |
| `get_git_settings` | `GET /api/git` | the `/git` page: commit identity, SSH keys (public halves), trust decisions |
| `save_git_identity` | `POST /api/git/identity` | commit name and email; reaches running pods at once |
| `generate_ssh_key` / `import_ssh_key` | `POST /api/git/keys`, `POST /api/git/keys/import` | a new ed25519 key, or a pasted unencrypted OpenSSH private key; installed in running pods at once |
| `delete_ssh_key` | `DELETE /api/git/keys/{id}` | removed from running pods at once |
| `forget_repo_trust` | `POST /api/git/trust/forget` | the user is asked again next time the remote is cloned |
| `list_secrets` (`api::secrets`) | `GET /api/secrets` | the `/secrets` page: `SecretSummary` rows — id, name, dates and `shown`, D7's display prefix (at most four leading characters of the value, at least eight bytes always hidden, plus the byte length), computed at read; no value ever leaves this endpoint (SME-242) |
| `save_secret` (`api::secrets`) | `POST /api/secrets/save` | `id` null creates (a fresh name — `git-ssh` refused; 1–65,536 bytes, refused empty), `id` set replaces that row's value only (names are immutable); every successful save runs the live-pod install |
| `delete_secret` (`api::secrets`) | `POST /api/secrets/delete` | removes the row and runs the install, so granted pods lose the file at once |
| `list_language_servers` / `get_language_server` | `GET /api/language-servers`, `GET /api/language-servers/{id}` | the Language servers page (SME-35) |
| `create_language_server` / `update_language_server` | `POST /api/language-servers`, `POST /api/language-servers/{id}` | validated; a rename or disable stops the server's pods |
| `delete_language_server` | `DELETE /api/language-servers/{id}` | stops its pods everywhere |
| `lookup_language_server` | `GET /api/language-servers/lookup/{package}` | a suggested config from mason's registry and Helix |
| `list_mcp_servers` / `get_mcp_server` | `GET /api/mcp-servers`, `GET /api/mcp-servers/{id}` | the `/mcp-servers` pages; header names only, never values (see [mcp.md](mcp.md)) |
| `mcp_server_status` | `GET /api/mcp-servers/{id}/status` | a real connection attempt: `Connected { tool_names }`, `Unreachable { error }`, `NotConnected` or `NeedsReconnect { error }` (an OAuth sign-in that can't be refreshed, SME-113) |
| `create_mcp_server` / `update_mcp_server` | `POST /api/mcp-servers`, `POST /api/mcp-servers/{id}` | update merges headers and drops the cached connection; both refuse a name holding `/` or `*`, which no tool rule could name it by (SME-241) |
| `delete_mcp_server` | `DELETE /api/mcp-servers/{id}` | also drops the cached connection |
| `start_mcp_server_oauth` | `POST /api/mcp-servers/{id}/oauth/start` | returns the authorization URL the browser goes to (SME-16) |
| `disconnect_mcp_server_oauth` | `POST /api/mcp-servers/{id}/oauth/disconnect` | clears the stored OAuth credentials |
| `take_mcp_server_oauth_error` | `POST /api/mcp-servers/{id}/oauth/callback-error` | the last failed callback's error, removed as it's read; the edit page calls it when the callback's redirect brought it there with `?oauth_callback=1` (SME-168) |
| `get_mcp_tool_classes` / `set_mcp_tool_class` | `GET`/`POST /api/mcp-servers/{id}/tool-classes` | SME-240: the edit page's Tool-classes section. GET returns one row per tool (`server_class` from the server's annotations, `annotated`, the user's `override_class` if any, `listed`) with a `server_note` when the live list is missing; POST upserts (`class: Some`) or clears (`class: None`, answered "no override was saved for {tool}" when nothing matched) one override, refusing blank names and class texts outside the three. |
| `list_sandbox_volumes` / `create_sandbox_volume` | `GET /api/sandbox-volumes`, `POST /api/sandbox-volumes` | volumes mounted into sandbox pods; create makes the PVC and expands a leading `~` in the mount path |
| `delete_sandbox_volume` | `DELETE /api/sandbox-volumes/{id}` | deletes the PVC, then the row |
| `delete_conversation` | `DELETE /api/conversations/{id}` | hard delete of the conversation and its whole subtree of agents (SME-236, see [api/agents.md](api/agents.md#deleting)); cascades to their messages (`ON DELETE CASCADE`); also tears down their sandboxes and browsing sessions, and drops its event channel and turn lock; runs on if the tab closes, in a task of its own (SME-262); deleting a nonexistent id is not an error |

Not yet implemented (a straightforward addition when needed): renaming a conversation.
