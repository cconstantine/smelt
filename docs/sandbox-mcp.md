# MCP tools from scripts in the sandbox

A script the model runs in its sandbox can call the conversation's MCP tools (SME-359), so a batch of calls, or a call whose result only feeds the next one, costs no model turns: the model reads the script's output once. Index: [docs/mcp.md](mcp.md) covers MCP servers themselves.

## How a script calls

Every terminal's `bash` has `$SMELT_MCP_SOCKET`, `/run/smelt/mcp.sock`: HTTP on a Unix socket the sandbox agent serves.

| Request | Answer |
| -- | -- |
| `GET /v1/tools[?server=<name>]` | `200 {"tools":[{"name":"<server>/<tool>","server","tool","class","description","input_schema"}]}` |
| `POST /v1/call {"tool":"<server>/<tool>","arguments":{…}}` | `200 {"content":[…],"structured_content":…,"text":"…"}`; the tool's own error is a `422` with `"is_error":true` |

`smelt-mcp` wraps it: `smelt-mcp tools [server]`, `smelt-mcp call <server>/<tool> ['<json>' | -]`. The JSON body goes to stdout; it exits 0 for a 2xx, 1 for a tool error, 2 for a refusal or a limit (403, 429), and 3 otherwise. It is the agent binary under another name (a link in `docker/sandbox/Dockerfile`), so it ships with the agent it talks to.

## The path a call takes

1. The agent reads the caller's pid from the socket (`SO_PEERCRED`) and walks `/proc` ppids up to a terminal's `bash`, whose current command the call is charged to; a process outside any terminal's tree (double-forked away) belongs to none.
2. It sends `mcp_request` (protocol 1.1, `src/agent_protocol.rs`) up the WebSocket smelt dialed to the pod, with a `call_id` of its own and the request as the script sent it. The agent reads neither the request nor the answer.
3. smelt answers with `mcp_response` on the same connection: the status and JSON body the script gets.

The agent's own answers: no connection from smelt is a `503` with `"outcome":"not_sent"`; a call waiting when the connection closes, or a newer one replaces it, or after 180 s, is a `503` with `"outcome":"unknown"` (the call may have run); more than 16 waiting is a `429`.

**After a smelt restart** smelt connects to every live pod's agent at startup (`sandbox::reconnect_live_pods`, SME-359's Q4), so a script's calls work again within the restart itself rather than after the model's next sandbox call. It never stops a pod it only couldn't reach: a death the cluster confirms is cleaned up, and any other failure leaves the pod for the next caller's reconnect to decide on (`ConnectMode::Startup`). A side effect, accepted: command exits the agent queued while smelt was down are delivered at startup and wake their conversations then.

No volume, a model's or a global one, may be mounted at or over `/run/smelt` or `/run`, so nothing can hide the socket: a model volume is held to all of `RESERVED_MODEL_MOUNT_DIRS`, and a global volume, the user's choice, to `/run` alone (`RESERVED_GLOBAL_MOUNT_DIRS`, checked at creation).

A Unix socket rather than a port: no port-forward, preview or `webfetch` route reaches it, and the pod never needs a network path to smelt. The pod's Docker containers can't reach it either, so a script that calls MCP tools runs in a terminal, not in `docker run`.

## smelt's side: the same rules as the model

`src/sandbox/script_mcp/` answers each `mcp_request` in a task of its own, so the connection's reader never waits on a call.

- **The connection is the identity.** A request carries no conversation or pod id (a body that names one is ignored): it is ruled as the conversation of the connection smelt itself dialed to that pod (`TerminalConnection`'s `conversation_id` and `pod_id`), so a pod can't call as another, and no token is needed.
- **First, the caller:** the pod must still be the conversation's live pod and the conversation not being deleted (`503` otherwise), and an agent conversation must be `active` (`403` once it is finished, stopped or failed). These checks, the turned-off check and the limits all come before the tool is looked up, so a refused or held-back call never connects to the server or refreshes its grant.
- **Then the call, exactly as the model's** (the user's answer to SME-359's Q1, "same as the model"): `anthropic::tools::call_mcp_for_script` reads the conversation's stored rule chain strictly (unreadable refuses, SME-354 B10) and `mcp::call_tool_raw` checks it at the class the connection's tool list and the user's override give, before anything is sent. A refusal is a `403` with the rule's own text. There is no separate ceiling for scripts.
- **The tool's name** is `<server>/<tool>` (the longest configured server name before a `/` wins) or the model's `mcp__<server>__<tool>`. A script calls only a tool its server lists, by its listed name (or the model's name for it): a name no configured server matches, or a tool the server doesn't list (another case, a zero-width character, an embedded `/` included), is a `404`, and nothing is sent. A body that isn't a JSON object with a `tool` string and object `arguments` a `400`; neither is recorded.
- **The result is the server's own:** every content block, `structured_content` and the joined `text`, not cut to the model's 30,000 characters, but bounded at 8 MiB of JSON (`502` beyond), and the `mcp_response` carrying it at 24 MiB once serialized (the body's escaping can double it); the agent takes WebSocket messages and frames up to 32 MiB, set explicitly. A failure below MCP is a `502`. Error text has the server's secrets redacted (`mcp::redact_secrets`: the URL's whole query and password, any query value or path segment that looks like a key, header values, OAuth tokens), on the model's calls too, and so is a tool's own error result (`isError`: its text blocks and every string in its `structured_content`, SME-386).
- **`GET /v1/tools`** lists what the model is offered (`mcp::offered_tools`, the same chain and class), with the servers' own names and input schemas.
- **Each call is a `sandbox_mcp_calls` row** ([database.md](database.md)), never holding its arguments or result (nothing prunes the table: a row goes when its pod or conversation is deleted, SME-386), and runs in a `script mcp call` span (conversation, pod, command, server, tool, class, status) over the call's `mcp call` span.

## Limits

A script's call costs no model tokens; its cost is the provider's quota, and for a writing tool the writes. The limits are smelt's, per conversation, in memory (`src/sandbox/script_mcp/limits.rs`; they reset when smelt restarts), each a `429`. A call held back writes no row, so a loop spinning on 429s can't fill the table: it is counted in memory per command, for the summary's "held back by a limit", and that count resets with smelt. A `tool` over 256 bytes is a `400`:

- **Rate:** `SANDBOX_MCP_CALLS_PER_MINUTE` (60), a token bucket with a burst of 20, taken by a call and by `GET /v1/tools` alike (a listing connects to every server); the answer carries `retry_after_secs`. `0` turns script calls off (a `403`, nothing recorded).
- **In flight:** `SANDBOX_MCP_MAX_IN_FLIGHT` (4) at once; one more is refused at once rather than queued.
- **Per command:** `SANDBOX_MCP_CALLS_PER_COMMAND` (1,000) calls, counted in `sandbox_mcp_calls`; calls traced to no command share a budget of the same size per pod, counted from an index on the pod's command-less calls. Waiting doesn't clear it: a new command starts a new budget. Two calls racing at the edge can both pass, so it can overshoot by up to the in-flight cap less one.

Each call also keeps the MCP call's own 120 s budget. The other half of a limit is being seen: each command's calls are summarized where the user and the model look (below).

## What the model and the user see

No call is a transcript entry of its own: a loop of 200 calls would flood the chat and cost the model a read of each, which is what this feature avoids. Instead each terminal command gets one line, built by `api::mcp_calls::McpCallSummary` from its `sandbox_mcp_calls` rows: `MCP: 14 calls · linear/list_issues ×12, linear/save_comment ×2 · 1 refused` (at most four tools named; then refusals, tool errors, failures, calls whose outcome is unknown, and calls held back by a limit). A call still `running` after 180 s (the agent's wait) counts as outcome unknown.

- **The model** gets the line as `mcp_calls` in `terminal_command_status` and `read_terminal_output`, for a command that made any calls. The completion notice ("Terminal command X finished") doesn't carry it yet (SME-374).
- **The user** sees the line under the command in the sandbox panel: it comes with the snapshot (`SandboxCommandSummary.mcp_calls`) and updates live through `ConversationEvent::SandboxMcpCalls`, published after a call finishes, at most every 500 ms per command and always after the last. Calls traced to no command are recorded and count against their pod's budget, but aren't shown in the panel yet (SME-375).
- **The system prompt's** environment section, under the MCP servers, says scripts can call their tools with `smelt-mcp`, that the conversation's tool rules apply, and the rate. The line is the same for every turn of a deployment, so the cached prompt stays put; it is left out with no servers, or with `SANDBOX_MCP_CALLS_PER_MINUTE=0`.

## What this doesn't cover

- **Stop and pause don't stop a running script's calls.** The conversation's Stop and pause end the model's turn, not its terminal commands: a script already running keeps calling, under the limits, until its command ends, is interrupted, or the pod stops. A call in flight when an agent finishes, or its pod stops, completes within its 120 s budget, as the model's own call would.
- **A background job's calls are charged to the terminal's current command.** Attribution walks the caller's process ancestry to a terminal's `bash` and takes that terminal's current command, so a job an earlier command left running (`cmd &`) counts against whichever command is current when it calls. A process double-forked out of the terminal's tree counts as outside any command.
- **A `--network host` container in the pod can reach the agent's loopback ports**, as it always could: the WebSocket on `127.0.0.1:8088` runs commands with no login, and that has been the pod's trust model since SME-33. The MCP socket itself is a file in the sandbox container, out of any Docker container's reach.
- **The system prompt's line appears for a pod on an older image too.** It is static per deployment, not per pod; in a pod whose image predates script calls, `smelt-mcp` isn't there, and once the pod is replaced it is.
