# Claude Code provider

Part of [setup.md](../setup.md).

A **Claude Code** provider (SME-399) runs its turns through the official, unmodified `claude` CLI on the smelt server's host, signed in to your Claude (Pro, Max, Team or Enterprise) subscription, so the subscription pays for them instead of per-token API billing.

**Turns don't run on it yet.** This first part (SME-429) adds the kind, its form, its model list and its sign-in status. A turn on it ends at once with "Claude Code providers can't run turns yet". The transport comes in SME-430, and compaction through it in SME-431.

## Anthropic's terms

Anthropic's Claude Code legal and compliance page lets individuals *"sign in to the unmodified Claude Code binary with your own Claude subscription, including where a platform hosts Claude Code."* smelt only starts that binary. It never reads or sends the subscription's token itself, and it never calls the API with it. Anthropic changed these terms three times in 2026, so this kind is the one that stops working if they change again. The provider form quotes them.

## The binary

The server image carries a pinned Claude Code at `/opt/claude/bin/claude`, on `PATH`, with `DISABLE_AUTOUPDATER=1` (see the [Dockerfile](../../Dockerfile)'s `claude` stage). The image build checks the release manifest's signature and the binary's checksum, and runs `claude --version`. Outside the image, install Claude Code yourself ([its setup docs](https://code.claude.com/docs/en/setup)) and leave the provider's binary blank for `claude` on the server's `PATH`, or give its full path.

## Signing in on a server with no browser

1. On your own machine, with Claude Code installed and your subscription, run `claude setup-token`. It opens the browser sign-in and prints a token that lasts about a year. It saves nothing.
2. Set that token as `CLAUDE_CODE_OAUTH_TOKEN` in the smelt server's environment (a deployment's Secret, or the dev `.env`), and restart the server.
3. Open the provider's page under **Model providers**. The line under **Models** says the CLI's version and "signed in with CLAUDE_CODE_OAUTH_TOKEN", with the plan when the CLI reports one. It reads `claude --version` and `claude auth status` each time the page loads, and never shows the token.

## What the CLI sees

smelt starts the CLI with a cleared environment and passes only these: `PATH`, `HOME`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR` and `DISABLE_AUTOUPDATER=1` (`claude_cli::cli_env`). Nothing else of smelt's reaches it. That matters for `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY`, which the CLI would prefer over the subscription (the dev `.env` sets the first, with `ANTHROPIC_BASE_URL`), and for `DATABASE_URL`. Its stderr is never read.

| Variable | Default | Notes |
|---|---|---|
| `CLAUDE_CODE_OAUTH_TOKEN` | unset | The subscription token from `claude setup-token`. Passed to the CLI only. Unset, the provider's page says "not signed in". |
| `SMELT_CLAUDE_CONFIG_DIR` | `/tmp/smelt-claude` | The CLI's `CLAUDE_CONFIG_DIR`: its settings and, once turns run, its sessions. Ephemeral by default. Mount a volume here to keep them across restarts. |

## Models

The CLI has no model listing, so the provider lists a fixed set: `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` and `claude-fable-5-1`, each with a 1,000,000-token window and a 128,000-token reply cap. Add another by id on the provider's page. Calls are priced on the `anthropic` price catalog entry, which the form picks: what the API would have charged, though the subscription pays.
