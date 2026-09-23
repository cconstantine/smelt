# Packaged, reusable skills

## What

A way to package a reusable set of instructions/context under a name the
model can invoke on demand — "load the X skill" — rather than everything
the model knows about how to do a task living permanently in one big
system prompt. Same idea as opencode's `skill` tool (and this harness's own
Skill mechanism).

## Why

`docs/projects/ideas/coding-session.md` asks for "a coding-oriented system
prompt so every conversation is a coding session by default" as the one
clearly open piece of that project. A single static system prompt works
for a general "act like a coding agent" instruction, but doesn't scale to
narrower, situational know-how (e.g. "how this specific repo's tests are
structured," "the house style for commit messages," "how to drive this
particular sandbox volume's contents") without either bloating every
conversation's context with instructions that are only relevant sometimes,
or leaving that knowledge unavailable entirely.

## Depends on

- The coding-oriented system prompt itself (`coding-session.md`'s open
  item) — a skill mechanism is a refinement on top of having a system
  prompt at all, not a replacement for starting one.
- Storage: a skill is a named bundle of text (and maybe files) — plausibly
  a small table (name, description, body) alongside the existing
  `mcp_servers`/`sandbox_volumes` config-table pattern in `db.rs`, or
  literally just files somewhere the sandbox can read, depending on
  whether skills are meant to be user-editable or code-shipped.
- Tool-definition cost: unlike a plain instruction in the system prompt, a
  `skill` *tool* means every skill's one-line description has to be sent
  with every request (so the model knows what's available to invoke) —
  same tradeoff `tool_definitions`'s MCP-tool-listing already makes today,
  just worth being deliberate about as the number of skills grows.

## Open questions

- Are skills smelt-authored/shipped (a fixed set describing this repo and
  common workflows) or user-authored/editable through a UI, mirroring how
  `/mcp-servers` and `/sandbox-volumes` are both user-configured?
- Does invoking a skill just inject its text into context, or can a skill
  bundle its own files/scripts into the sandbox (closer to opencode's
  richer skill packages)?
- Given how early "a coding-oriented system prompt" itself still is, this
  is probably premature until that lands — worth revisiting once
  `coding-session.md`'s open item is closed rather than pursuing both at
  once.
