# Setup & Running

## Commands

```bash
# ── Dev with hot reload (single command) ───────────────────────────────────
dx serve --fullstack
# Fullstack mode runs the real Axum server (SSR + server functions) and
# rebuilds/hydrates the WASM client on change, all through one process that
# dx manages — unlike a CSR-only app, there's no separate hand-rolled server
# process or web-mode proxy to wire up.
# dx binds an address itself (check its startup log); open that in a browser.

# ── Simplest one-shot run (no hot reload) ───────────────────────────────────
dx build --platform web
./target/dx/smelt/debug/web/server
# Run the *built* server binary, not `cargo run` — the server looks for the
# WASM client bundle in a "public" directory next to its own executable
# (override with DIOXUS_PUBLIC_PATH), and only `dx build`/`dx bundle`
# produce that layout. A plain `cargo run --features server` builds fine but
# panics on startup looking for a `public/` dir that was never created.

# ── Production ─────────────────────────────────────────────────────────────
dx bundle --platform web
# Produces a release server binary + WASM client bundle; see dx's output for
# the exact paths.

# ── Fast compile check ───────────────────────────────────────────────────────
cargo check --features server
cargo check --no-default-features --features web --target wasm32-unknown-unknown

# ── Tests ─────────────────────────────────────────────────────────────────
cargo test --features server
```

## Environment Variables

Copy `.env.example` to `.env` and fill in `ANTHROPIC_API_KEY`. Loaded
automatically at server startup (`dotenvy::dotenv()` in `main.rs`); a value
already set in the real environment takes precedence over `.env`. A var
that's set-but-empty is treated the same as unset (see `anthropic_model()`
and the `ANTHROPIC_API_KEY` check in `src/api/chat.rs`) — no code path
silently sends an empty string to the Anthropic API.


| Variable | Required | Default | Notes |
|---|---|---|---|
| `ANTHROPIC_API_KEY` | yes, to send messages | — | Read server-side only; the browser never sees it. Missing key surfaces as a `ChatEvent::Error` in the chat UI, not a crash. |
| `ANTHROPIC_MODEL` | no | `claude-opus-4-8` | Model id passed to the Messages API. |
| `ANTHROPIC_BASE_URL` | no | `https://api.anthropic.com` | Override for pointing at a mock upstream in tests, or an API-compatible gateway. |
| `DATABASE_URL` | no | `sqlite:./data/smelt.db` | SQLite path. |
| `PORT` | no | `8080` | Port the Axum server binds when run via plain `cargo run --features server` (not used by `dx serve`, which picks its own address). |
| `RUST_LOG` | no | (silent) | Standard `tracing-subscriber` env filter, e.g. `RUST_LOG=info,tower_http=debug`. |
