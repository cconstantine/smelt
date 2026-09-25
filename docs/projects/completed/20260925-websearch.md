# Web search via Exa's hosted MCP server

**Branch:** `websearch` · **Idea:** `projects/ideas/websearch.md` (removed) · **Plan:** `projects/plans/websearch.md` (removed)

## What shipped

The model can search the web. smelt adds Exa's hosted MCP server as a built-in MCP server at startup, and the model sees its search tool as `mcp__exa__web_search_exa` through the MCP client smelt already had. No search code of smelt's own, and no account: Exa's hosted MCP has a keyless, rate-limited mode. It's the same endpoint opencode's built-in web search uses.

- **`db::ensure_mcp_server(pool, name, url)`** inserts a server unless one with that name exists (`ON CONFLICT (name) DO NOTHING`).
- **`mcp::ensure_default_servers`**, called in `main()` after migrations, runs it for each built-in server. Today that's `("exa", EXA_MCP_URL)`. A failure is a logged warning, not a startup failure.
- **`EXA_MCP_URL` is `https://mcp.exa.ai/mcp?tools=web_search_exa`.** Keyless Exa offers search and fetch by default; the `tools=` parameter limits it to search, so reading a page stays with `webfetch`/`http_request` and their network guard.
- **Behavior:** a fresh database gets the entry on first start. A user's edits (an `x-api-key` header to lift the free limits, a new URL, OAuth) are kept, since only the name is matched. A deleted entry comes back on restart, which is what "always there" meant. Tests don't see it: test databases never run `main()`.
- **Verified for real:** a live check through smelt's own `rmcp` client (keyless connect, only `web_search_exa` offered, a real search returning URLs), and a real conversation where the model called `mcp__exa__web_search_exa` and answered with the version and its sources. The live check is `#[ignore]`d and also needs `SMELT_LIVE_EXA=1`, since CI's browser job runs every ignored test.

### Considered and dropped

A native `websearch` tool with Exa, Kagi and Brave adapters was planned first. Checking the market ruled out Google and Bing (Bing's API was retired in August 2025; Google's Custom Search API is closed to new customers and ends January 2027). Then opencode's source showed keyless Exa over MCP, and a custom tool's remaining benefits (always offered even when Exa is unreachable, our own tool description and result caps, testable in CI) didn't justify the code for one provider. It's worth revisiting if the MCP route is annoying in practice. The plan's three-provider version is in this branch's history (`e55ede8`).

### Not done

- **No off switch.** Deleting the entry doesn't stick; turning search off means editing its URL or tool list. A disabled flag on MCP servers would be its own small change.
- **Exa's keyless limits aren't published.** Going over them shows up as a tool error. Nothing measured yet.
- **If Exa's endpoint is unreachable, search silently disappears for that turn** (the MCP client skips unreachable servers). This is the main thing a custom tool would fix.
- **The browser tier now connects to Exa locally.** It shares the dev database, which has the entry, so every turn in that tier lists Exa's tools over the internet. It still passes in the same time. CI's fresh database has no entry.

## Retrospective

**What worked:**
- **Checking claims against the source before building on them.** I first answered the provider questions from memory. When asked to check, three of those claims were wrong or stale (Kagi's access and price, Brave's free tier, Marginalia's keyless access), while the ones about Bing and Google held up.
- **Reading how opencode actually does it.** Its source showed the keyless MCP endpoint, which no provider's own pricing page mentions and which I'd have missed. That one fact cut the project from three adapters to about 60 lines.
- **Asking whether a custom tool earns its keep.** Listing concretely what a native tool buys over configuration made the smaller design an easy call.
- **Watching the live check fail first.** Run against the unrestricted URL, it showed smelt's client would also offer `web_fetch_exa`. That confirmed the `tools=` parameter really works through smelt's own client, not just `curl`.

**What caused friction, surprise, or rework:**
- **The first answers about providers came from memory,** presented as fact with a general "check before choosing" caveat. It took a question from you to get them checked, and a plan (the three-provider one) was written on top of them before the keyless option turned up.
- **The MCP route was offered in the first answer and set aside** for "less say over results", which turned out not to matter much for one provider. The design only settled once the tradeoff was spelled out concretely.
- **`pkill -f "dx serve --port 8081"` killed its own shell,** because the pattern was also in the command line running it. Find a process by PID (`ps` with a `[d]x`-style pattern) before killing it.

**Process change (confirmed and applied to development-process.md, Plan phase step 4):**
- In the Plan phase: when a plan depends on an external service (availability, pricing, access terms, API shape), check each claim against the provider's current docs before presenting it, and cite the source; and look at how a comparable tool (opencode, for this project) does it in its source. Here, both would have got to the final design several rounds sooner.

**Bug bash:** not due. This is the first project since the 2026-09-24 bug bash.
