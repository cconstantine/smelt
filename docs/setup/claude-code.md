# Claude Code provider

Part of [setup.md](../setup.md).

A **Claude Code** provider (SME-399) runs its turns through the official, unmodified `claude` CLI on the smelt server's host, signed in to your Claude (Pro, Max, Team or Enterprise) subscription, so the subscription pays for them instead of per-token API billing.

SME-429 added the kind, its form, its model list and its sign-in status; SME-430 runs its turns (below). smelt can't yet compact a Claude Code conversation (SME-431): one that fills the model's window ends its turn saying so.

## Anthropic's terms

Anthropic's Claude Code legal and compliance page lets individuals *"sign in to the unmodified Claude Code binary with your own Claude subscription, including where a platform hosts Claude Code."* smelt only starts that binary. It never reads or sends the subscription's token itself, and it never calls the API with it. Anthropic changed these terms three times in 2026, so this kind is the one that stops working if they change again. The provider form quotes them.

## The binary

The server image carries a pinned Claude Code at `/opt/claude/bin/claude`, on `PATH`, with `DISABLE_AUTOUPDATER=1` (see the [Dockerfile](../../Dockerfile)'s `claude` stage). The image build checks the release manifest's signature and the binary's checksum, and runs `claude --version`. Outside the image, install Claude Code yourself ([its setup docs](https://code.claude.com/docs/en/setup)) and leave the provider's binary blank for `claude` on the server's `PATH`, or give its full path.

## Signing in on a server with no browser

1. On your own machine, with Claude Code installed and your subscription, run `claude setup-token`. It opens the browser sign-in and prints a token that lasts about a year. It saves nothing.
2. Set that token as `CLAUDE_CODE_OAUTH_TOKEN` in the smelt server's environment (a deployment's Secret, or the dev `.env`), and restart the server.
3. Open the provider's page under **Model providers**. The line under **Models** says the CLI's version and "signed in with CLAUDE_CODE_OAUTH_TOKEN", with the plan when the CLI reports one. It reads `claude --version` and `claude auth status` each time the page loads, and never shows the token.

## How a turn runs

Each turn starts one `claude -p` process (`claude_cli::run::TurnCli`), with stream-json in and out, every built-in tool, setting source, skill and slash command off, and smelt's system prompt (`--system-prompt-file`, re-read each launch). Each of the turn's model calls is one of its API calls.

- **Tools.** Claude Code runs every tool itself, so smelt shows it its tools through one MCP server, `smelt`, on a loopback listener inside the smelt server (`claude_cli::bridge`), with a random path and token per run. The model's call reaches the bridge as an MCP `tools/call`, which waits there while smelt's own turn loop runs the tool, as on every other kind; smelt's next call answers it with the result. The transcript keeps smelt's tool names.
- **Context.** The CLI's session holds the model's context; smelt's history stays the record. Each turn's process resumes the conversation's session (`--resume`, recorded in `claude_cli_sessions`) and is sent only what's new since its last call, so the prompt stays cached and earlier thinking carries over. A history that went another way (an edit, a switch from another provider), a failed call, or a session that won't resume (the config dir lost with a restart) starts a new session, given smelt's history as text: one uncached prompt, and the thinking lost.
- **Starts** are one at a time: every process shares the config dir, and each writes its `.claude.json` as it starts, so a start waits until the last one has begun (at most 10 s).
- **Thinking** is always on, and its text is hidden: the CLI sends thinking blocks with their signatures but no text, and neither `MAX_THINKING_TOKENS=0` nor its `alwaysThinkingEnabled` setting turns it off. The model's thinking setting has no effect here. **Effort** goes as `--effort`.
- **A reply cut off** at its limit (**Max reply tokens**, sent as `CLAUDE_CODE_MAX_OUTPUT_TOKENS`) is the turn's, as on HTTP. The CLI would carry on by itself with up to three more calls; smelt interrupts it at once, and records the usage of any call it had already started.
- **Stop, `ask_user` and the end of a turn** interrupt the process, which notes any tool call still waiting as stopped, then close it.
- **Errors.** A refused sign-in ends the turn saying to set `CLAUDE_CODE_OAUTH_TOKEN`; the subscription's usage limit says which limit and when it resets; anything else quotes the CLI. A CLI silent for 180 seconds is stopped.
- **Usage** is recorded per API call from the CLI's own events, priced as the API would have charged (the context view says the subscription pays).

## What the CLI sees

smelt starts the CLI with a cleared environment and passes only these: `PATH`, `HOME`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR` and `DISABLE_AUTOUPDATER=1` (`claude_cli::cli_env`), plus, for a turn, `DISABLE_COMPACT=1` (smelt's history is the record, and smelt compacts), `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MAX_MCP_OUTPUT_TOKENS=100000` and `MCP_TOOL_TIMEOUT` (a day: smelt bounds its own tools). Nothing else of smelt's reaches it. That matters for `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY`, which the CLI would prefer over the subscription (the dev `.env` sets the first, with `ANTHROPIC_BASE_URL`), and for `DATABASE_URL`. The status commands' stderr is never read; a turn keeps the last 2 KB of its process's stderr, for the error when the process dies.

| Variable | Default | Notes |
|---|---|---|
| `CLAUDE_CODE_OAUTH_TOKEN` | unset | The subscription token from `claude setup-token`. Passed to the CLI only. Unset, the provider's page says "not signed in". |
| `SMELT_CLAUDE_CONFIG_DIR` | `$HOME/.local/state/smelt-claude` | The CLI's `CLAUDE_CONFIG_DIR`: its settings, its sessions (`projects/`, one per conversation), each turn's working directory (`work/<conversation id>`) and system prompt file (`prompts/`). Lost, the sessions only cost each conversation's next turn a new session from its history. smelt makes it mode 0700 and refuses one someone else owns, since the CLI runs settings it finds there. It is never under a shared `/tmp`, where another user could make it first. In the server image it is lost on restart. To keep it, mount a volume at a parent and point this at a subdirectory of it: a volume's own root is rarely owned by smelt's user (k3s local-path makes it root-owned), so the chmod to 0700 fails and every turn and status check refuses it (SME-433). |

## Models

The CLI has no model listing, so the provider lists a fixed set: `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` and `claude-fable-5-1`, each with a 1,000,000-token window and a 128,000-token reply cap. Add another by id on the provider's page. Calls are priced on the `anthropic` price catalog entry, which the form picks: what the API would have charged, though the subscription pays.

## Recording the fixtures

The transport's tests run a fake CLI (the test binary itself) that replays lines recorded from the real one, in `src/claude_cli/fixtures/`. `scripts/claude-cli-capture <out dir> <scenario>...` records them: each scenario but `sanitize` is one real `claude -p` call on the host's own Claude Code sign-in, built only with the `claude-cli-capture` feature, so nothing in the gate runs it. `sanitize` writes the fixtures from the raw output with every id, date, address, signature and email replaced. Keep the out dir outside the repo and delete its `config/` after.
