# MCP servers that need to run alongside the sandbox

## What

Now that smelt can talk to external MCP servers (see
`docs/projects/completed/20260817-mcp-servers.md`), the model still can't use an MCP
server that needs to operate directly on files inside its own sandbox — a
git MCP server, for instance, has to read and write the actual checkout, not
just answer requests over a URL. This idea is smelt running that kind of MCP
server itself, alongside the model's sandbox, so those tools become
available too.

## Why

Some capabilities are exposed as MCP servers designed to sit right next to
the files they operate on. No amount of configuring external server URLs
gets the model that — the server has to actually run somewhere with real
access to the checkout. Git is the motivating example:
`docs/projects/ideas/coding-session.md` already flags "git clone/credential
wiring" as an open gap in giving smelt a real coding-agent workflow. Solving
it via a hosted MCP server means the model gets git access as a natural
extension of MCP support generally, rather than smelt hand-writing a
bespoke set of git tools.

## Depends on

The MCP client work — `docs/projects/completed/20260817-mcp-servers.md`,
already shipped — reuses the protocol client and the tool-dispatch path it
establishes, rather than being designed from scratch.

## Open questions

- How does a hosted server get real access to the sandbox pod's filesystem
  without smelt's own server process ever being the thing directly touching
  pod state — the same boundary the sandbox terminal/file tools already
  hold?
- Is a hosted server always available once the sandbox exists, or does the
  model (or the user) choose to start one?
- A hosted git server still needs real credentials (an SSH key or token) to
  do anything against a real remote — this idea gets the server running and
  its tools callable, not credentialed for a real push. That's a separate
  gap either way.
