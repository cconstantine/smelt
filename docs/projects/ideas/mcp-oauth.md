# OAuth authentication for MCP servers

## What

Right now smelt can only authenticate to an MCP server with static header
values the user pastes in themselves (see
`docs/projects/completed/20260817-mcp-servers.md`'s `extra_headers` column) — nothing
expires them, nothing refreshes them, and obtaining the value in the first
place is on the user. This idea is smelt supporting real OAuth for MCP
servers that need it: the user logs in once through a normal OAuth flow, and
smelt keeps the resulting credential fresh without anyone handling a token
by hand.

## Why

Some MCP servers are built around OAuth specifically rather than a static
API key or PAT. GitHub's own hosted MCP server, once thought likely to
require it, turned out not to — it accepts a plain static `Authorization:
Bearer <token>` header (confirmed with a real, working connection; see
`docs/projects/completed/20260817-mcp-servers.md`) — but other servers may
still require real OAuth. Static headers also rot on their
own: a token that expires or gets revoked silently breaks every MCP tool
call from that server until someone notices and pastes in a new one. Real
OAuth support fixes both — servers that require it become usable at all,
and "my token quietly expired" stops being a failure mode for the ones that
support OAuth's refresh flow.

## Depends on

The MCP client and `/mcp-servers` management UI —
`docs/projects/completed/20260817-mcp-servers.md`, already shipped — this
extends that page's add/edit flow with an OAuth option alongside static
headers, not a replacement for it (some servers will still only need a
static header).

## Open questions

- Where does the OAuth callback land — does smelt need its own redirect
  URI, or a small in-page flow inside `/mcp-servers`?
- How is the resulting refresh token stored — the same plaintext-in-Postgres
  question the static-header design already raises, sharper here since a
  refresh token is longer-lived and more sensitive than a single pasted
  value.
- Does every MCP server speak the same OAuth flow (MCP has its own auth
  spec), or does this need per-server handling?
