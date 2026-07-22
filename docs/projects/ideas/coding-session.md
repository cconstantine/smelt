# Coding session

## What

Give smelt the tools and system prompt to act as a coding agent — checking
out a repo, reading and editing files, running shell commands — as the
app's normal way of operating, not a toggle a user picks per conversation.
Every one of those actions (`git checkout`, file edits, command execution)
happens inside an isolated sandbox, never against the smelt server's own
filesystem/process.

## Why

smelt's purpose is to be a coding agent, not a general-purpose chat app
that also happens to offer a coding mode. Every conversation is a coding
session; there's no separate "plain chat" use case to preserve alongside
it. `state.md`'s goals already call out tool-use (filesystem/shell) as the
natural next project — this idea is that project, described as the app's
actual direction rather than an optional add-on.

Sandboxing isn't an afterthought here: the moment tool calls can run shell
commands and touch files, smelt's own server process is the thing an
unreviewed model response would be acting through. The server must never
be the thing that runs `git checkout`, writes a file, or execs a command —
a sandbox does, and the server only talks to the sandbox.

## Depends on

`anthropic::types::ContentBlock` needs `tool_use`/`tool_result` variants,
and `send_message`'s conversation loop (`src/api/chat.rs`) needs to
round-trip them, before there's anything to attach tools to. Most of the
wire types were deliberately kept close to the full Anthropic shape for
exactly this — see `state.md`.

## Sandboxing

Each coding session gets its own isolated, disposable environment. Nothing
the agent does — `git checkout`, file reads/writes, shell commands — ever
touches the smelt server's own filesystem or process directly; the server
only ever talks *to* the sandbox (start it, send it a command, read back
the result), never acts as it.

- **Isolation mechanism:** a container per session is the natural fit —
  smelt already assumes Docker for its own dev/deploy story (`Dockerfile`,
  `docker-compose.yml`), so provisioning one short-lived container per
  coding session (via the Docker API/socket, not shelling out to the CLI)
  reuses infra the project already depends on rather than introducing a
  new one (chroot/bubblewrap/VM). Revisit if per-session container
  spin-up latency turns out to matter.
- **Lifecycle:** one sandbox per conversation, created on first tool use
  (not eagerly on conversation create) and torn down when the conversation
  is deleted or after an idle timeout — needs to line up with whatever the
  delete-conversations idea lands on, so a deleted conversation doesn't
  leave an orphaned container running.
- **Filesystem:** a per-session working directory/volume the sandbox owns;
  `git checkout` (or `git clone`) of the target repo happens *inside* it,
  not on the host. No mounting of the smelt server's own source tree or
  host paths into the sandbox.
- **Network:** default to no egress, or an explicit allowlist (needs at
  minimum wherever `git` is cloning from) — the sandbox running arbitrary
  model-directed shell commands is exactly the thing that shouldn't have
  open internet access by default.
- **Resource limits:** CPU/memory/disk caps and a hard wall-clock timeout
  per command, so a runaway or malicious tool call can't starve the host
  smelt itself runs on.
- **Credentials:** git auth (for private repos) needs to reach the sandbox
  without smelt's own `ANTHROPIC_API_KEY`-style env-var-on-the-host pattern
  leaking a token into a container the model can run arbitrary commands
  in — this needs its own answer, not a reuse of how the Anthropic key is
  handled today.

## Visibility

A sandbox the user can't see into is worse than no sandbox — they need to
watch what the agent is actually doing (which commands ran, what output
they produced, which files changed) as it happens, not just receive a
final assistant message once the tool loop finishes.

- `send_message` already streams to the browser via `ServerEvents<ChatEvent>`
  (`ChatEvent::Delta`/`Done`/`Error`, see `architecture.md`'s "Two streams"
  section) — the natural extension is new `ChatEvent` variants for tool
  activity (e.g. a command starting, its live stdout/stderr, a file diff)
  emitted alongside the existing text deltas, so tool calls render inline
  in the same stream as the assistant's words rather than as a separate
  polling mechanism.
- Command output should stream as it's produced, not buffer until the
  command exits — sandboxed commands can be long-running, and a silent UI
  during a multi-second `cargo build` inside the sandbox reads as hung.
- File edits are more legible as a diff than as the raw `tool_use` JSON
  input — worth rendering a before/after diff in the transcript rather
  than dumping the tool call's parameters.
- Persistence: today `Message.content` is plain text (`models.md` notes
  this was deliberately kept simple since v1 has exactly one content
  shape). Tool activity needs to survive a page reload / reopening an old
  conversation, not just live-stream once — this likely means storing
  more than plain text per message once `ContentBlock` grows tool
  variants, or a separate table keyed by message/conversation for tool
  events. Needs a real answer at plan time, not deferred as "ephemeral is
  fine."

## Rough shape

- `send_message` always builds its `CreateMessageRequest` with a
  coding-oriented `system` prompt and the tool set attached — `system` is
  already `None` today, so this is additive, not a rewrite of the
  request-building path. No per-conversation branching, no `mode` field on
  `models::Conversation`.
- Tool execution is a new server-side component that proxies `tool_use`
  calls into the session's sandbox and returns the result as
  `tool_result`, **and** emits the visibility events described above as it
  goes — this is the piece that owns "never touch the host filesystem" and
  "the user can see what's happening," so it's worth designing and
  reviewing on its own before wiring it into the conversation loop.
- Start with a small, real tool set (bash + file read/write) rather than
  reimplementing Claude Code's full toolset (edit, glob, grep, ...)
  up front; grow it once the round-trip through `ContentBlock` and the
  sandbox proxy are solid.

## Open questions

- Confirmation/approval UX for destructive tool calls, on top of the
  sandbox itself — a sandboxed `rm -rf` is still destructive *within* the
  session's own checkout. See the security notes on client-side tools in
  the Claude API skill.
- Where does the sandboxed checkout come from — does the user paste a repo
  URL per conversation, or is smelt scoped to one project per deployment?
  Affects the "clone inside the sandbox" step above.
- Does `docs/projects/state.md`'s framing ("AI chat agent talking to
  Claude") and out-of-scope list need updating to reflect that coding is
  the intended purpose, not a future add-on? Worth revisiting when this
  moves from idea to plan.
