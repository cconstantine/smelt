# Feature checklist

Every shipped feature, with how to check it through the web UI. Each project's close-out adds or updates its rows; every bug bash runs the whole list in a browser (no seeding the database or calling APIs directly) and marks each row **works**, **broken**, **model** (the model didn't cooperate; retry once with a more explicit prompt) or **not web-checkable**. See [Bug bash](../process/sweeps.md#bug-bash-every-few-projects).

Tips: keep each model turn to one small ask when the model is slow. Clean up everything created, through the UI, and confirm `/pods` shows nothing left.

The list moved here from a Linear document on 2026-10-18, so the product's docs all live in the repo (see [Where work is tracked](../development-process.md#where-work-is-tracked)). Rows link tickets by id: an `SME-N` is a Linear ticket.

| File | What it covers |
| -- | -- |
| [Conversations and the chat page](conversations.md) | Conversations, the transcript, replies, stopping, questions and tabs. |
| [Sandboxes](sandbox.md) | Sandbox pods, terminals, files, Docker, dev servers, previews and their isolation. |
| [Tools and browsing](tools.md) | The model's tools: todos, search, fetch, the browser. |
| [Models and providers](models.md) | Model providers, a model per conversation, budgets, caching and costs. |
| [MCP, git and language servers](integrations.md) | MCP servers and their sign-in, git keys and repos, AGENTS.md, language servers. |
| [Dev tooling and operations](dev-and-ops.md) | CI, connection limits, check servers, the cluster lock, sandbox images, traces. |
| [Bug bashes and design reviews](sweeps.md) | What each bug bash fixed and design review 1. |

## Adding or changing a row

* A ticket's close-out adds or updates its row in the file for its area, on the ticket's branch, so the row ships in the same PR as the feature. Append new rows at the end of the table.
* One row per ticket: `| SME-N | what a user can do | how to check it in the browser |`. Don't hard-wrap a row: a table row is one line.
* No file fits: add one, with the same heading and table, and list it above. Keep each file under the size ceiling (see [rules-commits.md](../process/rules-commits.md)).
* A bug bash runs every file and records each row's mark in the bug bash's ticket, not here.
