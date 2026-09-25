# Web search via Exa's hosted MCP server

**Branch:** `websearch` · **Idea:** `projects/ideas/websearch.md`

## What

Give the model web search by making Exa's hosted MCP server a built-in MCP server: smelt inserts it into `mcp_servers` at startup if it isn't already there. The model then sees Exa's `web_search_exa` tool as `mcp__exa__web_search_exa`, through the MCP client smelt already has. No new tool code and no account: Exa's hosted MCP has a keyless mode ("free rate-limited usage without sign-in or API key"). A keyless `tools/call` from this environment returned good results (title, URL, highlights) on 2026-09-25. This is the same endpoint opencode's built-in websearch uses.

Going beyond the free limits needs no code either: add an `x-api-key` header to the entry on `/mcp-servers` (Exa's MCP accepts that, or OAuth).

A custom `websearch` tool with Exa, Kagi and Brave adapters was planned first and dropped for now. It's worth revisiting if the MCP route is annoying in practice: search disappearing from a turn when Exa's endpoint is unreachable, Exa's tool descriptions not fitting alongside `webfetch`, or needing results capped.

## How

- **`db::ensure_mcp_server(pool, name, url) -> Result<bool, sqlx::Error>`**: `INSERT INTO mcp_servers (name, url) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING`, returning whether it inserted. Keyed on `name`, which is already `UNIQUE`. Every other column keeps its default: no extra headers, `static_headers` auth.
- **At startup, in `main()`**, right after migrations: `ensure_mcp_server(pool, "exa", EXA_MCP_URL)`, logging when it inserts. A failure is logged as a warning and doesn't stop the server; search is an extra, not a requirement.
- **Where the default lives:** a small `default_mcp_servers()` list in `src/mcp.rs` (name and URL), so a second default later is one line, and a test can check the list itself.
- **Behavior this gives:**
  - A fresh database gets the entry on first start.
  - An entry the user **edited** (added an API key header, changed the URL, switched to OAuth) is left alone, since only the name is matched.
  - An entry the user **deleted** comes back on the next start. That's what "always there" means here; see the open questions.
  - Tests are unaffected: `#[sqlx::test]` databases run migrations, not `main()`, so no test suddenly has an external MCP server in its tool list.
- **Why startup and not a migration:** a migration runs once, so a deleted entry would stay deleted and there'd be no "always there".

## Tests (test-first)

- `ensure_mcp_server` inserts a missing server; a second call inserts nothing and returns `false`; an existing entry with a changed URL and headers is left exactly as it was.
- `default_mcp_servers()` contains the Exa entry with the chosen URL.
- **Live check, `#[ignore]`d** (it needs the internet and Exa's service, so it isn't in CI): smelt's own MCP client connects to the Exa entry keylessly, lists its tools (`web_search_exa` present), and a real `web_search_exa` call returns text containing a URL. This is the part not proven yet: the earlier check used `curl`, not smelt's `rmcp` client, which does the full MCP handshake.
- **Manual check in the running app:** after a restart, `/mcp-servers` lists `exa`, and a real conversation asked to look something up calls `mcp__exa__web_search_exa` and uses the results.

## Which files

- `src/db.rs`: `ensure_mcp_server` plus its tests.
- `src/mcp.rs`: `default_mcp_servers()`, `EXA_MCP_URL`, the live check.
- `src/main.rs`: the startup call.
- `docs/setup.md` or `docs/architecture.md`: note that `exa` is added at startup, and how to add a key.
- Close-out: completed doc, `docs/projects/state.md` (the web tools feature line and the "on hold" note in Goals), removing `docs/projects/ideas/websearch.md` and this plan.

## Open questions and tradeoffs

1. **Which Exa tools to expose.** The keyless defaults are `web_search_exa` and `web_fetch_exa`, and the URL's `tools=` parameter picks which ones are offered. Proposal: **`https://mcp.exa.ai/mcp?tools=web_search_exa`**, search only, so reading a page stays with `webfetch`/`http_request` and the model isn't choosing between two fetch tools. The alternative is to keep Exa's fetch too, as a lighter reader than a real browser.
2. **A deleted entry comes back on restart.** That follows from "always there", but it means there's no way to turn Exa off short of editing its URL. If you want an off switch, a disabled flag on MCP servers would be its own small change. Proposal: leave it as is for now.
3. **Keyless limits aren't published.** A 429 will show up as an MCP tool error the model can see. If it happens often in real use, add a key through the `/mcp-servers` page.
4. **Queries go to Exa.** Every search query is sent to a third party, unauthenticated in keyless mode. That's inherent to any search provider; noted so it's a conscious choice.
