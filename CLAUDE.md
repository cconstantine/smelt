# smelt

A single-user, 100%-Rust AI chat agent talking to Claude. Dioxus fullstack (SSR + hydration) on Axum, SQLite via sqlx, streamed replies via Dioxus's native `ServerEvents` SSE payload type — no hand-rolled REST layer, no hand-written browser fetch client.

## Docs

| Topic | File |
|---|---|
| Build, run, env vars | [docs/setup.md](docs/setup.md) |
| Module map, request flow, feature flags | [docs/architecture.md](docs/architecture.md) |
| sqlx pool, query pattern, `db::get()` | [docs/database.md](docs/database.md) |
| Server functions (`#[get]`/`#[post]`), `send_message`/`ServerEvents` streaming | [docs/api.md](docs/api.md) |
| `Conversation`/`Message` structs | [docs/models.md](docs/models.md) |
| Dioxus components, routing, calling server functions from the UI | [docs/frontend.md](docs/frontend.md) |
| Inline tests, mock-upstream SSE testing | [docs/testing.md](docs/testing.md) |
| New feature flow, plan phase, TDD workflow | [docs/development-process.md](docs/development-process.md) |
| Current features, architecture, goals | [docs/projects/state.md](docs/projects/state.md) |
| Completed projects | [docs/projects/completed/](docs/projects/completed/) |
| Pending project ideas | [docs/projects/ideas/](docs/projects/ideas/) |
