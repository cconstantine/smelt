# Current state

What smelt does today, how it's built, and what is next. This is the project's
**Current state** document, in files rather than one Linear document, so a
session can edit one section without rewriting the others (SME-159). Read this
one first; the sections are:

* [features.md](features.md) — what the app and the agent do, feature by feature
* [agents.md](agents.md) — child agents: what a conversation's model hands off, and how it hears back
* [bug-bashes.md](bug-bashes.md) — what each bug bash swept and fixed
* [architecture.md](architecture.md) — how the code is laid out, short version;
  [docs/architecture.md](../architecture.md) is the full picture

## What smelt is

A single-user, 100%-Rust AI coding agent. Every conversation is a coding session: the model writes, runs and fixes code in a sandbox pod of its own, through tools, and the user watches and steers it from a browser chat. It's a Dioxus fullstack (SSR + hydration) web app talking to Claude via the Anthropic Messages API (or a compatible endpoint), with Postgres-persisted conversation history, token-by-token streamed replies, and a system prompt that tells the model how its sandbox and tools work (see [api.md](<https://github.com/cconstantine/smelt/blob/main/docs/api.md>)). No login system. Where the model runs is set up in the app: model providers (Anthropic, Ollama, llama.cpp, or any Anthropic-compatible server) with their keys, kept server-side, and a model per conversation (SME-72).

## Explicitly out of scope (v1)

* Every tool is real (the `run_async` task suite and its stand-ins `add`/`count`/`echo` were removed by SME-54; long-running work goes through terminal commands, and smelt never stops one on a timer — it tells the model it's still running and lets the model decide, SME-79) — SME-28's leftover is SME-78 below (repos and credentials in the sandbox shipped with SME-32).
* Multi-user accounts or login.
* Renaming conversations. (Deleting is supported — see [api.md](<https://github.com/cconstantine/smelt/blob/main/docs/api.md>).)
* A single shell dying (or the agent itself) while the rest of the pod survives isn't specifically detected — per SME-12's finding, a real OOM kill on this cluster always takes the whole pod down together, so this narrow case (something other than OOM — a bash bug, a stray signal — killing just one shell) is a real but lower-priority gap, not the common case.
* Sandbox volumes have no upload UI and no single-file (Secret-backed) kind yet — a volume starts empty and only ever gets content from the model's own `write_file`/`edit_file` tools once it's mounted into a live pod. Both arrive together in a later pass; see SME-17's "What shipped."

## Goals for what comes next

The open tickets, by theme (Backlog unless marked):

* **What the model can do:** SME-78 (stop idle sandbox pods, Up Next), SME-73 (compact and carry on when a turn overflows the context, Up Next), SME-29 (model safety, including confirmation before destructive actions), SME-36 (a multi-file patch tool), SME-37 (skills), the rest of Personas and sub-agents (child agents shipped with SME-236, see [agents.md](agents.md); SME-38 was folded into it), SME-31 (MCP servers running alongside the sandbox).
* **Models and providers:** SME-162 (a provider's limit on concurrent requests, Up Next), SME-109 (pick a cost-effective provider), SME-110 (a ChatGPT plan), SME-74 and SME-98 (keep earlier reasoning across a model, prompt or tool change), SME-120 (thinking blocks whose text is omitted), SME-116 (reasoning priced higher reads low).
* **The UI:** SME-71 (desktop notifications, Up Next), SME-44 (VS Code into a sandbox), SME-138 (Copy over plain HTTP), SME-160 ("Working" timer, In Progress), and design review 2 (SME-123, Todo).
* **Security:** SME-45 (previews only for signed-in users), SME-47 (unprivileged Docker, blocked on the homelab node's Linux 5.10), SME-48 (SSH keys encrypted and out of the model's reach), SME-63 (Chrome's sandbox, pod service-account tokens, network policy), SME-128 (listen on loopback only), SME-131 (state-changing remote calls a timeout can cancel), SME-139 (a release build aborts on any panic).
* **Bugs:** bug bash 4's SME-165 to SME-190, and SME-136, SME-146, SME-148, SME-150, SME-151.
* **Structure:** SME-55 (typed statuses), SME-163 (a repo friendlier to LLM sessions, Up Next).
* **Tests, CI and ops:** SME-96 (check servers' previews, Up Next), SME-66, SME-87, SME-64, SME-50, SME-97, SME-101, SME-103, SME-124, SME-130, SME-133, SME-140, SME-141, SME-142, SME-145, SME-147 and SME-161 (flaky tests), SME-149, SME-152, SME-154, SME-158, SME-159, SME-164.
