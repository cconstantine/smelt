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

# ── Build sandbox_agent first, for any `server`-feature build ──────────────
# src/sandbox.rs's `include_bytes!` embeds a pre-built sandbox_agent binary
# at compile time — every `server`-feature build (check, test, or run) fails
# with "couldn't read .../sandbox_agent: No such file or directory" without
# this having run at least once since the last `cargo clean`. Not needed for
# the `web`-only wasm check (`sandbox` is `#[cfg(feature = "server")]`-gated).
scripts/build-sandbox-agent.sh

# ── Fast compile check ───────────────────────────────────────────────────────
cargo check --features server
cargo check --no-default-features --features web --target wasm32-unknown-unknown

# ── Tests ─────────────────────────────────────────────────────────────────
# Requires Postgres running first (docker compose up -d postgres) — DB tests
# use #[sqlx::test], which needs a reachable DATABASE_URL. See testing.md.
cargo test --features server
```

## CI

`.github/workflows/ci.yml` runs on every pull request: the full `cargo test
--features server` suite (Postgres + real-cluster k3s sandbox tests), the
WASM `cargo check`, and the automated browser tier — the same tests and the
same `docker-compose.yml` stack described above and in
[testing.md](testing.md), just running on GitHub's runner instead of a local
machine. See [development-process.md](development-process.md#definition-of-done).

## Environment Variables

Copy `.env.example` to `.env` and fill in `ANTHROPIC_API_KEY` (or
`ANTHROPIC_AUTH_TOKEN` — see below). Loaded automatically at server startup
(`dotenvy::dotenv()` in `main.rs`); a value already set in the real
environment takes precedence over `.env`. A var that's set-but-empty is
treated the same as unset (see `anthropic_model()` and
`require_at_least_one_credential()` in `src/api/chat.rs`) — no code path
silently sends an empty string to the Anthropic API.


| Variable | Required | Default | Notes |
|---|---|---|---|
| `ANTHROPIC_API_KEY` | yes, unless `ANTHROPIC_AUTH_TOKEN` is set | — | Read server-side only; the browser never sees it. Sent as the `x-api-key` header. Missing (with no `ANTHROPIC_AUTH_TOKEN` either) surfaces as a `ChatEvent::Error` in the chat UI, not a crash. |
| `ANTHROPIC_AUTH_TOKEN` | no | — | Alternative to `ANTHROPIC_API_KEY`, sent as an `Authorization: Bearer` header instead of `x-api-key` — for an Anthropic-compatible gateway that expects bearer auth (e.g. Hugging Face's hosted endpoint) rather than a real Anthropic API key. If both are set, `ANTHROPIC_AUTH_TOKEN` takes precedence (only one auth header is ever sent). At least one of the two must be set. |
| `ANTHROPIC_MODEL` | no | `claude-opus-4-8` | Model id passed to the Messages API. |
| `ANTHROPIC_BASE_URL` | no | `https://api.anthropic.com` | Override for pointing at a mock upstream in tests, or an API-compatible gateway — e.g. a local Ollama server (v0.14.0+ serves an Anthropic-compatible `/v1/messages`; see the commented-out example in `.env.example`). Pick a model with a large-enough context window for tool-calling to work — some models default to a much smaller one than they support. |
| `ANTHROPIC_THINKING` | no | on | Set to `0`/`false`/`off` to stop sending `thinking: {"type": "adaptive"}`. On by default — `run_turn` retries a request without thinking if the upstream fails with Ollama's specific "error parsing tool call" shape (seen with `gpt-oss` models, whose Anthropic-compat shim doesn't cleanly separate reasoning from a tool call's arguments), so this only needs turning off if some other backend hits a *different* thinking-related failure that retry doesn't cover. See [docs/api.md](api.md). |
| `DATABASE_URL` | yes | — | Postgres connection string; `db::init()` panics on startup if unset. Set in `docker-compose.yml`'s `smelt` service, pointing at the `postgres` compose service (only reachable from other compose services, not the host) — only needed in `.env` if running outside docker compose. |
| `PORT` | no | `8080` | Port the Axum server binds when run via plain `cargo run --features server` (not used by `dx serve`, which picks its own address). |
| `RUST_LOG` | no | (silent) | Standard `tracing-subscriber` env filter, e.g. `RUST_LOG=info,tower_http=debug`. |
| `KUBECONFIG` | yes, for sandbox code/tests | — | Read by `kube::Client::try_default()` (`src/sandbox.rs`). In `docker-compose.yml`, `smelt`'s `KUBECONFIG` points at the kubeconfig `k3s-bootstrap` generates for the `park` service account against the compose-provided `k3s` service — a hermetic test cluster, not a real deployment target. Point it at `.kubeconfig.yaml` (gitignored) instead to deliberately target the real `homelab` cluster. See [docs/projects/plans/k8s-sandbox.md](projects/plans/k8s-sandbox.md). |
| `SANDBOX_MEMORY_LIMIT` | no | `8Gi` | Default memory limit for a sandbox pod's container — a plain Kubernetes quantity string. Just the *default*: `create_pod`'s `memory_limit` parameter overrides it per pod, up to the `smelt-park` namespace's `LimitRange` ceiling (`k8s/smelt-park-rbac.yaml`). Hitting the limit kills the whole pod at once (`memory.oom.group=1` on this cluster), not just the offending process — see [projects/completed/20260816-sandbox-oom.md](projects/completed/20260816-sandbox-oom.md). |
| `SANDBOX_CPU_LIMIT` | no | `1` | Default CPU limit for a sandbox pod's container, same shape as `SANDBOX_MEMORY_LIMIT` (a Kubernetes quantity string, e.g. `"2"` for two cores) — overridable per pod via `create_pod`'s `cpu_limit`. |
| `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` | no | `30` | How long `wait_for_running` (`src/sandbox.rs`) waits for a pod to reach `Running` before giving up with `SandboxError::Timeout`. The default is plenty on the real `homelab` cluster or a resource-rich dev machine; a CPU-constrained CI runner schedules pods measurably slower, so `.github/workflows/ci.yml` raises this for its `cargo test --features server` run. |
| `SMELT_BASE_URL` | no | derived from the request | Overrides the scheme+host `src/mcp_oauth.rs` builds an MCP OAuth redirect_uri from (`/mcp-servers`' Connect flow). Without it, the base URL is derived from the incoming request's `Host` header (`X-Forwarded-Proto` for scheme) — wrong if smelt sits behind a proxy/tunnel that doesn't forward a `Host` a browser/OAuth provider could actually reach. No trailing slash. Set-but-empty is treated as unset, same as every other env var here. |
