# A coding-oriented system prompt

**Branch:** `system-prompt` · **Idea:** `projects/ideas/coding-session.md` (item 1 of "Still open")

## What

Every normal turn sends `system: None` today (`run_turn_bounded` in `src/api/chat.rs`); only compaction has a system prompt. So the model knows nothing about smelt beyond its tool descriptions:
- that it's a coding agent;
- that its sandbox is a Kubernetes pod it has to create first, running as the `sandbox` user in `/home/sandbox` with `sudo`;
- that commands run in the background and it's notified when they finish;
- which volumes are mounted where;
- that the user reads its replies as plain text, not rendered markdown;
- today's date.

This adds a system prompt to every turn, so smelt acts as a coding agent by default. That was the idea's first open item.

## How

### Shape: fixed base, plus a generated environment section

The same split opencode uses, checked in its source (`packages/opencode/src/session/system.ts` and `prompt/anthropic.txt`): a fixed base prompt, and an environment block built per turn.

- **Base prompt: `src/api/system_prompt.md`**, compiled in with `include_str!`. It's a text file so it can be read and edited as prose. Proposed sections:
  1. **Who you are:** smelt, a coding agent. The user works with you in a browser chat, and every conversation is a coding session.
  2. **Your sandbox:**
     - A pod you create with `create_pod` before using terminals or files.
     - At most one per conversation.
     - Runs as `sandbox` in `/home/sandbox`, with `sudo` for installing things (e.g. `sudo apt-get install -y git`).
     - Your files live only in the pod and in mounted volumes. A terminated pod loses everything outside volumes.
  3. **Running commands:**
     - `run_terminal_command` starts a command and returns immediately.
     - You're notified when it finishes, so don't poll in a loop. Use `read_terminal_output` or `terminal_command_status` if you need output sooner, and `send_signal` to stop one.
     - Only one command at a time per terminal; open another terminal for parallel work.
  4. **Files:**
     - Prefer the file tools (`read_file`/`edit_file`/`write_file`/`list_directory`/`glob`/`grep`) over shell equivalents: they show the user a diff.
     - `edit_file`, and `write_file` when overwriting, need the hash from a fresh `read_file`.
  5. **The web:** `mcp__exa__web_search_exa` (when configured) to find pages, `http_request` for APIs, `webfetch` for pages that need JS, and a browsing session when you need to click through a site. The user can watch and use the browsing session too.
  6. **Working style:**
     - Use `todowrite` for multi-step work.
     - Verify changes by running them (build, tests) rather than assuming.
     - Say plainly what you did and didn't verify.
     - Ask before destructive actions the user didn't ask for, even inside the sandbox (deleting files, `git reset --hard`, dropping data).
     - Be concise.
  7. **Output:** replies show as plain text with line breaks kept, not rendered markdown. Short paragraphs and `-` lists read fine; avoid tables and heavy markdown. Code goes in plain fenced blocks.
- **Environment section**, generated per turn and appended as its own block:
  - today's date (UTC);
  - the model ID in use (`anthropic_model()`);
  - the configured sandbox volumes, name → mount path (from `db::list_sandbox_volumes`; omitted when there are none);
  - the configured MCP servers by name, so the model knows which `mcp__<server>__…` tools come from where.

  All of this changes rarely, so the prompt stays identical across turns within a day. That matters if prompt caching is added later.

### Code

- `src/api/chat.rs`:
  - `fn system_prompt(env: &PromptEnvironment) -> String`: pure, joining the base with the rendered environment block.
  - `async fn prompt_environment(pool) -> PromptEnvironment`: reads the date, model, volumes and MCP server names. A failed database read leaves that line out and logs it, rather than failing the turn.
  - `run_turn_bounded` sets `system: Some(system_prompt(&env))`, built once per turn.
  - `get_context_detail` returns the same prompt, from the same function, so the detail view's "System prompt" section (already rendered when `system` is `Some`) shows exactly what's sent.
- Compaction keeps its own `COMPACTION_SYSTEM_PROMPT`.
- No token-budget change: the real `usage` numbers the compaction trigger uses already count the system prompt, since it's part of the request.

## Tests (test-first)

- **The request carries it:** a mock upstream that records request bodies (the existing mock doesn't); a turn's request has `system` equal to `system_prompt(...)` for that conversation's environment.
- **Environment rendering:** date and model always present; volumes listed as name → path; the volume and MCP lines omitted when empty; each database read failing on its own leaves just that line out.
- **The detail view matches:** `get_context_detail`'s `system` equals what the turn request sends.
- **No drift between prompt and tools:** every tool name the base prompt mentions in backticks exists in `native_tool_definitions()` (MCP names excepted). Renaming or removing a tool fails the test until the prompt is updated.
- **Compaction is unaffected:** the summarization request still uses `COMPACTION_SYSTEM_PROMPT`.
- **Manual check with a real model**, before and after, on a few representative asks:
  - "clone <a public repo> and run its tests": does it create a pod, install git, and wait for notifications instead of polling?
  - "what's today's date?"
  - a formatting-heavy question: are replies readable as plain text?

  Recorded in the completed doc.

## Which files

- `src/api/system_prompt.md`: new, the base prompt.
- `src/api/chat.rs`: `PromptEnvironment`, `prompt_environment`, `system_prompt`, the request and detail-view changes, and tests (including a request-recording mock).
- `docs/api.md`: the system prompt section (what's in it and where it's built).
- Close-out:
  - `docs/projects/completed/YYYYMMDD-system-prompt.md`;
  - `docs/projects/state.md`: the Goals paragraph naming this gap, and the framing of "What smelt is" (the idea's item 6, if you agree; see open question 4);
  - `docs/projects/ideas/coding-session.md`: remove item 1;
  - removing this plan.

## Open questions and tradeoffs

1. **Your own standing instructions?** opencode and Claude Code read project files (`AGENTS.md`/`CLAUDE.md`). smelt has no project checkout yet (the idea's item 2), so there's nothing to read. Proposal: not in this project. A per-deployment "extra instructions" setting (an env var, or a text field on a settings page) could come next if you want one now.
2. **Asking before destructive actions** is proposed as a prompt rule only (section 6), not an approval mechanism. That's cheap, and it partly addresses the idea's item 5 without building confirmation UI.
3. **Markdown:** tell the model to write plain text (proposed; matches today's rendering), or render markdown in the chat instead and let it write normally? Rendering is the better long-term answer but a separate UI project.
4. **Update `state.md`'s "What smelt is" to say coding agent** as part of this close-out (the idea's item 6)? Proposal: yes, it's one paragraph.
5. **Prompt caching** (`cache_control` on the system prompt and tools) would cut cost on long conversations. Out of scope here; the stable-within-a-day prompt keeps the door open.
