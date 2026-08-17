# MCP (Model Context Protocol) client support

**Branch:** `mcp-servers` · **Plan:** `projects/plans/mcp-servers.md` (removed)

Not from a dedicated idea file — a redirect mid-conversation from
`projects/ideas/coding-session.md`'s open "git clone/credential wiring"
item. The user chose MCP over the sandbox-mounted-credential approach that
idea file originally sketched: "Let's work on the git access, but do it
via mcp... we really need to add support for is MCP servers."

## What shipped

A client-only MCP integration (smelt connects out to externally-hosted MCP
servers; smelt hosting its own is `projects/ideas/mcp-hosted-servers.md`,
still just an idea) built on the official `rmcp` SDK's Streamable HTTP
transport:

- **`src/mcp.rs`** — connection registry keyed by `mcp_servers.id`, lazy
  connect with a one-retry safety net (`retry_once`) for a transient first
  attempt, real handling of server-initiated `tools/list_changed`
  notifications (invalidates the cached tool list rather than requiring a
  config edit to refresh), `mcp__<server>__<tool>` name-space translation.
- **`mcp_servers`** Postgres table + `src/db.rs` CRUD, including a
  merge-based `update_mcp_server_config` (upsert/remove by header name)
  rather than a wholesale replace — the browser never receives header
  *values* back (write-only, only names), so a caller can change or drop
  one header without ever knowing the others' real values.
- **`src/api/mcp.rs`** server functions, and `anthropic::tools::
  tool_definitions`/`execute` gained an MCP dispatch path alongside the
  existing native tools.
- **`/mcp-servers` UI** — three pages: an index with a live per-server
  "Connected — N tools" / "Unreachable" badge (a real connection attempt,
  not a cached guess), a create page, and a single-form edit page (name,
  URL, and in-place header editing — add/remove/modify any header without
  retyping the others) with a three-state Save button (unchanged/disabled,
  changed/active, saving/busy-spinner) and the same busy-spinner treatment
  on the status Refresh button.
- **Proven end-to-end** against GitHub's real hosted MCP server
  (`https://api.githubcopilot.com/mcp/`) — a live `mcp__Github__get_me`
  call round-tripped to the user's real GitHub account.

## Two real bugs found only against the real app

- **TLS didn't work at all initially** — `rmcp`'s
  `transport-streamable-http-client-reqwest` Cargo feature pulls a bare
  `reqwest` with no TLS backend; that's gated behind a separate `reqwest`
  feature name on `rmcp` itself. Every local test passed (the mock MCP
  server in `src/mcp.rs`'s own tests uses a plaintext in-memory transport,
  never real HTTP) right up until a real `https://` connection failed at
  runtime with `scheme is not http`.
- **Fixing that broke 10 unrelated `sandbox::` tests** — enabling `rmcp`'s
  `reqwest` feature pulled in a second `rustls` crypto-provider candidate
  via Cargo feature unification on the shared `reqwest` dependency,
  breaking `rustls`'s implicit single-candidate auto-detection wherever
  `CryptoProvider::install_default()` hadn't been called explicitly.
  `main()` already called it before building any TLS client; `sandbox.rs`'s
  test helper didn't, so only `cargo test` broke, never the real binary.

## An unresolved-but-mitigated flakiness

Two real conversations (separately) failed to see GitHub's tools right
after the server was configured, with nothing logged (`RUST_LOG` is
silent by default). The first was fully explained — the `mcp_servers`
table was genuinely empty at the time (an earlier diagnostic row had been
added and then deliberately cleaned up, and never re-added with a real
token). The second had a configured, working row the whole time, and a
direct repro immediately after each failure always succeeded — never
reproduced on demand. See the retrospective below for the leading theory,
which was never proven. Regardless of cause, `ensure_connected` now
retries a failed connection once before giving up for that turn — cheap,
safe, and covers the actual failure shape either way.

## Retrospective

**What worked:**
- TDD held for every piece of real logic — the merge-based header update
  (`upsert` wins over `remove` for the same name, a deliberate and tested
  choice), the `retry_once` helper (generic and unit-testable without a
  real network call), `tools/list_changed` cache invalidation. Each was
  written failing-first and shown failing for the right reason before the
  real implementation.
- Live browser verification (via `scripts/browser-check`, since
  `/opt/playwright-venv` wasn't present in this sandbox despite being
  documented) caught a real CSS bug — a long connection-error message
  overflowing the page horizontally — that no unit test would have caught.
  Root cause was non-obvious: the status box inherited `white-space:
  nowrap` from a shared badge class it reused for a different display
  mode.
- A throwaway MCP server (an always-unreachable URL) for every live UI
  test, deleted immediately after, kept the user's real GitHub-token-
  holding row untouched through dozens of manual verification passes.
- Several rounds of short, one-sentence UX feedback ("let me edit them
  [instead of replace]," "single form, single save button," "spinner on
  refresh too") were each implemented fully — including a real backend
  redesign from wholesale-replace to merge-based headers — without needing
  a clarifying question, and none needed correction afterward.

**What caused friction, surprise, or rework:**
- The MCP-tools-missing investigation never reached a confirmed root
  cause for its second occurrence. The leading theory: my own diagnostic
  edits to `db.rs`/`mcp.rs` during the *same* investigation were
  themselves triggering `dx serve`'s automatic rebuild-and-restart —
  indistinguishable in effect from the explicit-restart case
  `development-process.md` already warns about, except nothing was
  *explicitly* restarted. A plain edit-test-edit debugging loop against a
  file the dev server watches can interrupt a concurrent real user
  request the same way killing the process would. This was never proven
  with a captured error — the failure simply stopped recurring once the
  investigation itself stopped generating concurrent edits.

**What to change (confirmed and applied to `development-process.md`):**
- Extended the existing "ask before killing/restarting a process the user
  started" rule to name the *implicit* version of the same hazard:
  investigating a live-app symptom by editing source files in a repo
  `dx serve` is actively watching can interrupt a concurrent real request
  exactly like an explicit restart would, with no `kill` command involved
  at all. If a live symptom stops reproducing right around when a
  debugging session stops actively editing files, that's a specific
  signal worth naming rather than reading as the bug self-healing.
