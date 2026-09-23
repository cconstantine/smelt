# A web search tool (websearch)

## What

A tool that runs a query against a search API and returns results, giving
the model a way to discover URLs worth looking at (not just fetch ones it
already knows) — same idea as opencode's `websearch`. Split out from
`webfetch.md` — see that file's "Split out" note for why.

## Why

`webfetch` alone only helps once the model already knows a specific URL.
Plenty of research questions ("what changed in this crate's latest
release," "what's the current recommended approach for X") start from a
query, not a URL — `websearch` closes that gap the same way a human
developer opening a search engine would.

## Depends on

- A real search API and a credential for it (Exa, Brave, Parallel, Tavily,
  etc. — opencode uses Exa/Parallel) — a new external credential in the
  same shape as `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`
  (`docs/setup.md`'s env var table), not something already provisioned in
  this environment. **Which provider to use, and setting up an account/API
  key for it, is a decision only the user can make** — this is why this
  idea is on hold rather than immediately scoped into a plan.
- Same result-shaping concern as `webfetch`: a search result set needs to
  come back bounded and readable (titles/URLs/snippets), not a raw API
  response dump.

## Open questions

- Which provider — cost, quality, and whether it offers a snippet/summary
  per result or just a URL list, all affect the tool's shape.
- Whether results should be fetchable directly (an inline "and also fetch
  the top N" option) or should always be a separate explicit `webfetch`
  call per URL the model decides is worth reading.
