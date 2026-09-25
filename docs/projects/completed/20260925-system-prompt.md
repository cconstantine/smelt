# A coding-oriented system prompt

**Branch:** `system-prompt` · **Idea:** `projects/ideas/coding-session.md` (item removed) · **Plan:** `projects/plans/system-prompt.md` (removed)

## What shipped

Every turn now carries a system prompt; before this, normal turns sent none. It has two parts, the same split opencode uses:

- **A fixed base prompt**, `src/api/system_prompt.md`, compiled in with `include_str!`. It covers:
  - smelt as a coding agent;
  - the sandbox: create the pod first, the `sandbox` user in `/home/sandbox`, passwordless `sudo`, a minimal Debian image, and what's lost when a pod ends;
  - running commands: they return at once, a "finished" message arrives on its own, so don't poll (and `wait_task` is only for `run_async` tasks);
  - preferring the file tools, and reading before editing;
  - which web tool to use when;
  - working style: answer concept questions directly and use the sandbox for building, running and changing things; todos; verify by running; say what was and wasn't verified; be concise;
  - replies are shown as plain text, so no markdown in any reply.
- **An environment section**, built for each request: today's date (UTC), the model, the configured volumes with their mount paths, and the configured MCP servers. A failed database read leaves its line out and is logged.

The context detail view shows the exact prompt a turn sends, from the same `system_prompt`/`prompt_environment` pair. Compaction keeps its own prompt. `state.md` now describes smelt as a coding agent.

Following the plan review, the prompt has **no "ask before destructive actions" rule**; that belongs to `projects/ideas/model-safety.md`. Replies stay **plain text**; rendering markdown is `projects/ideas/markdown-in-chat.md`. Both are placeholders for you to scope.

**Tests:**
- the request carries the prompt (via a new request-recording fake API server);
- the environment section renders, including leaving out empty lines;
- `prompt_environment` reads volumes and MCP servers;
- the detail view matches what's sent;
- compaction keeps its own prompt;
- a drift test: every tool the base prompt names in backticks must exist.

Each was seen failing first. The drift test caught a deliberately misspelled `create_pods`; the compaction test caught its request deliberately given the wrong prompt.

The prompt is rebuilt for each model call within a turn, not once per turn as the plan said. That costs two small queries per call.

### Checked with a real model

The model under test was the one configured in `.env`: `Qwen/Qwen3.8-27B` through featherless, not Claude. Three asks, first with the prompt switched off, then with it on, then again after tuning:

| Ask | No prompt | First prompt | Tuned prompt |
|---|---|---|---|
| "What's today's date?" | wrong: July 24, 2026 | right: 2026-09-25 | (not re-run) |
| Clone `pallets/itsdangerous` and run its tests | worked; ran `apt-get` without `sudo` first, then probed for `sudo`; polled status, tried `wait_task` | 297 passed in 211s; 7 status polls, 2 × `wait_task`, `**bold**` in the summary | 297 passed in 151s; 3 status checks, no `wait_task`, no markdown |
| Compare three ways to share state between threads in Rust | answered; headings, bold, tables | **created a pod and started installing Rust** for a demo; unfinished after 3 minutes | answered directly in 64s; still 4 headings and a table |

What the tuning changed: an explicit "after starting a command, don't check on it" (naming both tools it misused), "not every message needs the sandbox", and "no markdown in any reply, including summaries". The first prompt's sandbox overreach only showed up in the live check; no unit test would have caught it.

### Not done

- **This model still uses headings and tables on explanation-style answers.** The plain-text rule halved the problem but didn't end it. Rendering markdown (`markdown-in-chat.md`) is the real fix. Claude may follow the rule better; not measured.
- **Your own standing instructions** (a `CLAUDE.md`-style file or setting): not in this project, since there's no project checkout to read one from yet.
- **Prompt caching** (`cache_control`): out of scope. The prompt only changes with the date or configuration, so it's ready for it.

## Retrospective

**What worked:**
- **The before/after check with a real model.** It proved the prompt's value (the date, `sudo`, fewer wasted calls), and it found a problem the prompt itself caused (reaching for the sandbox on a concept question) that no test could have.
- **The drift test.** A prompt that names tools goes stale silently when a tool is renamed; now it fails loudly.
- **The plan review.** Your two decisions (no destructive-action rule, plain text only) kept this project small, and split two real topics into their own ideas instead of half-doing them here.

**What caused friction, surprise, or rework:**
- **`git add -A` swept your in-progress files into my commit.** You added `pod-management.md` and a line in `coding-session.md` while I worked. Nothing had been pushed, so I split them out, but only because I happened to read the commit afterward.
- **Killing by pattern went wrong twice.** Once the kill command matched its own shell. And since `websearch`, the retro's advice ("find by PID with a `[d]x` pattern") wasn't enough: the second time, the restart in the same command line matched. The pattern also matched by binary path, so it could have hit your server's process; it didn't only because your server's binary happened to have a different name.
- **My edits restarted your dev server.** It watches the source files, so each prompt edit (and the temporary prompt-off edit) rebuilt and restarted it. development-process.md already describes this hazard. I noticed only when I saw your server running.
- **My check script had no time limit** while waiting for a conversation to go quiet, and sat for 13 minutes after the clone task had finished. Fixed with a hard budget and a per-poll log.

**Process suggestions (not applied; need agreement):**
- **Stage files by name, never `git add -A` or `git commit -a`**, since you may be editing the repo at the same time.
- **Stop only processes you started, by the PID recorded when you started them.** Never kill by name or pattern. Before editing source files for a live check, look for a dev server of yours watching the repo, and say so if one is running.

**Bug bash:** not due. Two projects since the 2026-09-24 bug bash.
