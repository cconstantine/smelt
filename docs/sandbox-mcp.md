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

A Unix socket rather than a port: no port-forward, preview or `webfetch` route reaches it, and the pod never needs a network path to smelt. The pod's Docker containers can't reach it either, so a script that calls MCP tools runs in a terminal, not in `docker run`.
