# Current state

What smelt does today, how it's built, and what is next. This is the project's
**Current state** document, in files rather than one Linear document, so a
session can edit one section without rewriting the others (SME-159). Read this
one first; the sections are:

* [features.md](features.md) — what the app and the agent do, feature by feature
* [architecture.md](architecture.md) — how the code is laid out, short version;
  [docs/architecture.md](../architecture.md) is the full picture

## What smelt is

A single-user, 100%-Rust AI coding agent. Every conversation is a coding session: the model writes, runs and fixes code in a sandbox pod of its own, through tools, and the user watches and steers it from a browser chat. It's a Dioxus fullstack (SSR + hydration) web app talking to Claude via the Anthropic Messages API (or a compatible endpoint), with Postgres-persisted conversation history, token-by-token streamed replies, and a system prompt that tells the model how its sandbox and tools work (see [api.md](<https://github.com/cconstantine/smelt/blob/main/docs/api.md>)). No login system. Where the model runs is set up in the app: model providers (Anthropic, Ollama, llama.cpp, or any Anthropic-compatible server) with their keys, kept server-side, and a model per conversation (SME-72).

## Explicitly out of scope (v1)

* Every tool is real (the `run_async` task suite and its stand-ins `add`/`count`/`echo` were removed by SME-54; long-running work goes through terminal commands) — see SME-28 for what's still open on top of them (repos and credentials in the sandbox have since shipped with SME-32).
* Multi-user accounts or login.
* Renaming conversations. (Deleting is supported — see [api.md](<https://github.com/cconstantine/smelt/blob/main/docs/api.md>).)
* A single shell dying (or the agent itself) while the rest of the pod survives isn't specifically detected — per SME-12's finding, a real OOM kill on this cluster always takes the whole pod down together, so this narrow case (something other than OOM — a bash bug, a stray signal — killing just one shell) is a real but lower-priority gap, not the common case.
* Sandbox volumes have no upload UI and no single-file (Secret-backed) kind yet — a volume starts empty and only ever gets content from the model's own `write_file`/`edit_file` tools once it's mounted into a live pod. Both arrive together in a later pass; see SME-17's "What shipped."

## Goals for what comes next

The open tickets, by theme (Backlog unless marked Todo):

* **What the model can do:** SME-28 (the rest of a coding session: deleting idle pods, a per-command time limit), SME-29 (model safety, including confirmation before destructive actions), SME-36 (a multi-file patch tool), SME-37 (skills), SME-38 (subagents), SME-31 (MCP servers running alongside the sandbox), SME-73 (compact and carry on when a turn overflows the context).
* **The UI:** SME-44 (VS Code into a sandbox, Todo).
* **Security:** SME-45 (previews only for signed-in users), SME-47 (unprivileged Docker, blocked on the homelab node's Linux 5.10), SME-48 (SSH keys encrypted and out of the model's reach), SME-63 (Chrome's sandbox, pod service-account tokens, network policy).
* **Structure, from bug bash 3's review:** SME-55 (typed statuses), SME-57 (split the chat page: the components are split into `pages/chat/` in its first PR; the per-conversation store, the reducer, render performance and SME-58's chat-page items follow), SME-58 (frontend cleanups), SME-62 (robustness leftovers).
* **Tests, CI and ops:** SME-66 (real-cluster tests for pypi/cargo installs), SME-87 (why dx's server build sometimes hangs in CI; since SME-67 the job stops it and carries on), SME-68 (a flaky browser test), SME-64 (language servers on the homelab cluster), SME-50 (a pod-exec script).
