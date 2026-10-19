# smelt

A single-user, 100%-Rust AI chat agent talking to Claude. Dioxus fullstack (SSR + hydration) on Axum, Postgres via sqlx, streamed replies via Dioxus's native `ServerEvents` SSE payload type — no hand-rolled REST layer, no hand-written browser fetch client. The few exceptions (the `/oauth/mcp-callback/{id}` route, the `request_guard` middleware, the sandbox preview proxy's own listener) are in [docs/architecture.md](docs/architecture.md#stack).

## Docs

| Topic | File |
|---|---|
| Build, run, deploy, env vars | [docs/setup.md](docs/setup.md) |
| Module map, request flow, feature flags | [docs/architecture.md](docs/architecture.md) |
| sqlx pool, query pattern, `db::get()`, schema (every table) | [docs/database.md](docs/database.md) |
| Server functions (`#[get]`/`#[post]`), `send_message`/`ServerEvents` streaming | [docs/api.md](docs/api.md) |
| `Conversation`/`Message` and `LanguageServer*` structs | [docs/models.md](docs/models.md) |
| MCP servers (client, tools, OAuth) | [docs/mcp.md](docs/mcp.md) |
| Dioxus components, routing, calling server functions from the UI | [docs/frontend.md](docs/frontend.md) |
| Where tests live, database and sandbox tests, running them, the browser tier | [docs/testing.md](docs/testing.md) |
| The development process: plan, TDD, rules, done, close-out (an index of its parts) | [docs/development-process.md](docs/development-process.md) |
| Personas (Project Manager, Architect, Planner, Developer, Code Reviewer, Integrator, Facilitator) and the ticket workflow they run | [docs/personas/README.md](docs/personas/README.md) |

## Project tracking

Ideas, plans and completed work are tickets in Linear (team "Smelt Agent", ids `SME-N`). Every ticket belongs to a project, one per line of work (a feature, a bug bash, a cleanup), with true one-offs in "Maintenance". Linear holds only tracking; product and process documents live in the repo, including the [Feature checklist](docs/feature-checklist/index.md). Statuses: Backlog = idea, Up Next = to plan, Planned = plan awaiting the user, Todo = approved and ready, In Progress, In Review = awaiting the user's answers to its questions, Approved = ready to merge, Done. Current features, architecture and goals are in [docs/current-state/](docs/current-state/index.md). An `SME-N` in a code comment or doc is one of these tickets. See [docs/development-process.md](docs/development-process.md#where-work-is-tracked).

## For any agent

These hold for every agent working on the repo, Claude or not.

- **Every tracked text file stays under 40,000 bytes and 2,000 lines** (about 11k tokens: one read in most agents' file tools, and a small part of a local model's window). `src/repo_lints.rs` checks it in `cargo test --features server`, which `scripts/check.sh` runs (a plain `cargo test` skips it). Aim new files at 30,000 bytes or less. When a file nears the ceiling, split it along its own sections: a module's tests go to `foo/tests.rs`, or `foo/tests/<topic>.rs` once they're big, and a big module becomes a directory of submodules. See [docs/process/rules-commits.md](docs/process/rules-commits.md#files-stay-under-the-ceiling).
- **Production functions stay under 200 code lines.** `scripts/lint-expects` runs clippy's `too_many_lines`, and `src/repo_lints.rs` checks `#[component]` functions, which clippy can't see into; split a long function along its own steps.
- **`scripts/check.sh` gates every commit:** both builds with no warnings, the lints, and the server tests. Tests that use the shared Postgres or the k3s cluster run under the cluster lock: through `scripts/check.sh`, or `scripts/cluster-test <cargo test arguments>` for one test, never a bare `cargo test` (see [docs/testing.md](docs/testing.md#running-tests)).
- **Never `git checkout` in the main checkout or touch the dev server:** work in a worktree of your own (see [docs/personas/rules.md](docs/personas/rules.md)).
- **Ticket work needs Linear** (the Linear MCP tools): plans and What shipped live in the ticket, retro notes on the standing "Next retrospective" ticket. The process starts at [docs/development-process.md](docs/development-process.md), and the personas that run it at [docs/personas/README.md](docs/personas/README.md).
