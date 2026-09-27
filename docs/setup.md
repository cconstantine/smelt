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

# ── Build + deliver the custom sandbox image, for real sandbox usage ───────
# `docker/sandbox/Dockerfile`'s ENTRYPOINT *is* the sandbox agent — it's no
# longer `include_bytes!`'d into the `smelt` binary at compile time, so a
# plain `cargo build`/`cargo test --features server` doesn't need this at
# all. But `create_pod` (real usage, or any real-cluster sandbox test) will
# fail — `ImagePullBackOff`, since this image only ever lives on the
# cluster's own node, never a real registry — until this has been run at
# least once against a live cluster (once `k3s-bootstrap` has completed;
# these commands already assume DOCKER_HOST/KUBECONFIG are set, same as
# every other command on this page). Safe, if slower than necessary, to
# re-run after any change; see
# SME-17. It also imports `docker:29-dind`, the image each pod's Docker
# sidecar runs (SME-33), the same way.
scripts/build-sandbox-image.sh

# ── Set up chrome-headless-shell, for real webfetch/browsing usage ─────────
# The `webfetch` tool, and the persistent browsing-session tools
# (`open_browser_session`/`browser_navigate`/... plus the live panel) built
# on the same shared browser, all navigate a real headless browser
# (`chromiumoxide`, driving `chrome-headless-shell` over CDP) — not just
# `src/browser_tests.rs`'s own test tier anymore. `scripts/browser-check/setup.sh`
# downloads the binary plus its missing shared libraries into
# `.browser-check-cache/` (gitignored) — no root needed, safe to re-run.
# Without this, any real `webfetch`/`open_browser_session` call fails with a
# clear "chrome-headless-shell not found" error rather than hanging or
# crashing. See SME-21 and
# SME-22.
scripts/browser-check/setup.sh

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
| `ANTHROPIC_API_KEY` | yes, unless `ANTHROPIC_AUTH_TOKEN` is set | — | Read server-side only; the browser never sees it. Sent as the `x-api-key` header. Missing (with no `ANTHROPIC_AUTH_TOKEN` either) surfaces as a `TurnError` in the chat UI, not a crash. |
| `ANTHROPIC_AUTH_TOKEN` | no | — | Alternative to `ANTHROPIC_API_KEY`, sent as an `Authorization: Bearer` header instead of `x-api-key` — for an Anthropic-compatible gateway that expects bearer auth (e.g. Hugging Face's hosted endpoint) rather than a real Anthropic API key. If both are set, `ANTHROPIC_AUTH_TOKEN` takes precedence (only one auth header is ever sent). At least one of the two must be set. |
| `ANTHROPIC_MODEL` | no | `claude-opus-4-8` | Model id passed to the Messages API. |
| `ANTHROPIC_BASE_URL` | no | `https://api.anthropic.com` | Override for pointing at a mock upstream in tests, or an API-compatible gateway — e.g. a local Ollama server (v0.14.0+ serves an Anthropic-compatible `/v1/messages`; see the commented-out example in `.env.example`). Pick a model with a large-enough context window for tool-calling to work — some models default to a much smaller one than they support. |
| `ANTHROPIC_THINKING` | no | on | Set to `0`/`false`/`off` to stop sending `thinking: {"type": "adaptive"}`. On by default — `run_turn` retries a request without thinking if the upstream fails with Ollama's specific "error parsing tool call" shape (seen with `gpt-oss` models, whose Anthropic-compat shim doesn't cleanly separate reasoning from a tool call's arguments), so this only needs turning off if some other backend hits a *different* thinking-related failure that retry doesn't cover. See [docs/api.md](api.md). |
| `ANTHROPIC_CONTEXT_WINDOW` | no | `200000` | Real token count for the configured model's context window — used to decide when auto-compaction should trigger and to compute the context-usage indicator's percentage. `context_window_for` already recognizes every current `claude-*` model id (all sharing the same standard 200K window) without needing this set; only a fallback for a gateway or local model (`ANTHROPIC_BASE_URL` pointed elsewhere) with no real Anthropic model id to look up. See [SME-18](https://linear.app/smelt-agent/issue/SME-18). |
| `DATABASE_URL` | yes | — | Postgres connection string; `db::init()` panics on startup if unset. Set in `docker-compose.yml`'s `smelt` service, pointing at the `postgres` compose service (only reachable from other compose services, not the host) — only needed in `.env` if running outside docker compose. |
| `PORT` | no | `8080` | Port the Axum server binds when run via plain `cargo run --features server` (not used by `dx serve`, which picks its own address). |
| `RUST_LOG` | no | (silent) | Standard `tracing-subscriber` env filter, e.g. `RUST_LOG=info,tower_http=debug`. |
| `KUBECONFIG` | yes, for sandbox code/tests | — | Read by `kube::Client::try_default()` (`src/sandbox.rs`). In `docker-compose.yml`, `smelt`'s `KUBECONFIG` points at the kubeconfig `k3s-bootstrap` generates for the `park` service account against the compose-provided `k3s` service — a hermetic test cluster, not a real deployment target. Point it at `.kubeconfig.yaml` (gitignored) instead to deliberately target the real `homelab` cluster. See [SME-7](https://linear.app/smelt-agent/issue/SME-7). |
| `SANDBOX_MEMORY_LIMIT` | no | `8Gi` | Default memory limit for a sandbox pod's container — a plain Kubernetes quantity string. Just the *default*: `create_pod`'s `memory_limit` parameter overrides it per pod, up to the `smelt-park`/`smelt-park-test` namespace's `LimitRange` ceiling (`k8s/smelt-park-rbac.yaml`). Hitting the limit kills the whole pod at once (`memory.oom.group=1` on this cluster), not just the offending process — see [SME-12](https://linear.app/smelt-agent/issue/SME-12). |
| `SANDBOX_CPU_LIMIT` | no | `1` | Default CPU limit for a sandbox pod's container, same shape as `SANDBOX_MEMORY_LIMIT` (a Kubernetes quantity string, e.g. `"2"` for two cores) — overridable per pod via `create_pod`'s `cpu_limit`. |
| `SANDBOX_IMAGE` | no | `docker.io/library/smelt-sandbox:latest` | The sandbox pod's image reference — `docker/sandbox/Dockerfile`, built and delivered with no registry involved by `scripts/build-sandbox-image.sh`. Must match whatever `ctr images import` actually registered the image as, not an arbitrary tag — see SME-17. |
| `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` | no | `30` | How long `wait_for_running` (`src/sandbox.rs`) waits for a pod to reach `Running` before giving up with `SandboxError::Timeout`, which carries the pod's own reason for still being pending (e.g. `PodScheduled: Unschedulable: persistentvolumeclaim … not found`) when it has one. A container stuck in a state that won't recover (an image pull failure, a crash loop) fails at once with `SandboxError::StartFailed` instead of waiting out the timeout. The default is plenty on the real `homelab` cluster or a resource-rich dev machine; a CPU-constrained CI runner schedules pods measurably slower, so `.github/workflows/ci.yml` raises this for its `cargo test --features server` run. |
| `SMELT_BASE_URL` | no | derived from the request | Overrides the scheme+host `src/mcp_oauth.rs` builds an MCP OAuth redirect_uri from (`/mcp-servers`' Connect flow). Without it, the base URL is derived from the incoming request's `Host` header (`X-Forwarded-Proto` for scheme) — wrong if smelt sits behind a proxy/tunnel that doesn't forward a `Host` a browser/OAuth provider could actually reach. No trailing slash. Set-but-empty is treated as unset, same as every other env var here. |
| `SANDBOX_DOCKER_MEMORY_LIMIT` | no | `8Gi` | Memory limit for each sandbox pod's Docker sidecar (SME-33). Every container Docker runs counts against it, not against `SANDBOX_MEMORY_LIMIT`; an OOM kill restarts only the sidecar. `create_pod`'s `docker_memory_limit` overrides it per pod, up to the same `LimitRange` ceiling. See [Docker in the sandbox](#docker-in-the-sandbox). |
| `SANDBOX_DOCKER_CPU_LIMIT` | no | `1` | CPU limit for the Docker sidecar, overridable per pod with `create_pod`'s `docker_cpu_limit`. |
| `SANDBOX_DOCKER_STORAGE_SIZE` | no | `20Gi` | Size each conversation's Docker data claim (`sandbox-docker-<conversation id>`, its images, build cache and named volumes) requests. |
| `SANDBOX_WORKSPACE_STORAGE_SIZE` | no | `20Gi` | Size each conversation's `/workspace` claim (`sandbox-workspace-<conversation id>`, its files and checkouts) requests. |
| `SANDBOX_DOCKER_IMAGE` | no | `docker.io/library/docker:29-dind` | The Docker sidecar's image, delivered into the node by `scripts/build-sandbox-image.sh`. Only its `dockerd` is used, never its entrypoint. |
| `SMELT_PREVIEW_URL` | no | `http://{port}-{conversation}.preview.localhost:8181` | The address a sandbox preview gets (see [Sandbox previews](#sandbox-previews)): a scheme and host holding `{port}` and `{conversation}` once each, with something other than digits between them, and no path. In production, e.g. `https://{port}-{conversation}-smelt.constantlee.us`. An invalid value turns previews off, logged as an error at startup; the rest of smelt runs as usual. |
| `SMELT_PREVIEW_ADDR` | no | `0.0.0.0:8181` | Where the preview proxy listens. It's a listener of its own, not a route on smelt's main port (see below). A port that can't be bound turns previews off, logged as an error. |

## Built-in MCP servers

At startup, smelt adds any built-in MCP server that isn't configured yet, matched by name (`mcp::ensure_default_servers`). Today that's one: **`exa`**, Exa's hosted MCP server at `https://mcp.exa.ai/mcp?tools=web_search_exa`, which gives the model web search (`mcp__exa__web_search_exa`) with no account or key. The `tools=` parameter limits it to search, so reading a page stays with `webfetch`/`http_request`.

- **Keyless by default.** Exa's free, rate-limited mode. Its limits aren't published; going over them comes back as a tool error. To lift them, add an `x-api-key` header with an Exa key to the entry on `/mcp-servers`.
- **Edits are kept.** Only the name is matched, so a changed URL, an added header or a switch to OAuth survives restarts.
- **Deleting doesn't stick.** A deleted entry comes back on the next start. To turn search off, change its URL or tool list instead.
- **Search queries go to Exa**, unauthenticated in keyless mode.

## Sandbox previews

A server running in a conversation's sandbox pod (a dev server, a web app) can be opened in two browsers, both through a Kubernetes port-forward to the pod, so a server bound to `127.0.0.1` inside the pod works too (SME-42):

- **The model's own browser.** In `webfetch` and browsing sessions, `http://localhost:<port>/` (also `127.0.0.1` and `[::1]`) is that port in the conversation's own pod. Every other loopback or private address is still refused.
- **The user's browser.** The model calls `sandbox_preview_url` with the port; the sandbox panel then shows an "Open preview · port N" link to `SMELT_PREVIEW_URL` for that port and conversation, e.g. `http://5173-42.preview.localhost:8181`.

Previews are served on a listener of their own (`SMELT_PREVIEW_ADDR`), not on smelt's main port: `dx serve`'s dev proxy replaces the `Host` header before a request reaches the app, and a preview's address is all in its `Host`.

- **Dev:** compose publishes port 8181. After pulling this change, recreate the `smelt` container from the host (`docker compose up -d smelt`) so the port is published; this restarts the container. Browsers resolve `*.localhost` to this machine with no DNS set up.
- **Production:** point the preview hostnames at `SMELT_PREVIEW_ADDR`'s port in the front end, with the same TLS and access rules as smelt itself. The flat `{port}-{conversation}-smelt.constantlee.us` form fits under an existing `*.constantlee.us` wildcard certificate.
- **Access:** smelt has no login, so a preview is exactly as protected as smelt is. Other websites open in the same browser can't send requests to a preview (or, in the model's browser, to the sandbox): only the conversation's own previews and opening the link directly get through. A later login can cover previews too; SME-42 records how.

## Docker in the sandbox

Every sandbox pod runs its own Docker daemon (SME-33), so the model can `docker build`, `docker run` and `docker compose up` in any terminal, as on a Linux machine:

- **A sidecar.** dockerd runs in a privileged `docker` container next to the unprivileged `sandbox` container, which reaches it through a socket shared at `/run/docker-sock` (`DOCKER_HOST`). It never listens on TCP: the pod's containers share one network, so a TCP port would be reachable through a preview.
- **Shared files.** `/workspace` is shared by both containers, and terminals start there. Docker resolves bind mounts in its own container, so a bind mount from anywhere else, `~` included, gives the container an empty directory.
- **Its own limits.** Containers count against the sidecar's limits (`SANDBOX_DOCKER_MEMORY_LIMIT`/`SANDBOX_DOCKER_CPU_LIMIT`). The sidecar starts dockerd with `docker/sandbox/start-dockerd.sh`, not the `docker:dind` entrypoint: a privileged container shares the node's cgroup tree, and that entrypoint rearranges the node's root cgroup and lets containers escape every limit. An OOM kill restarts only the sidecar; the model gets a notice, and its terminals and `/workspace` carry on.
- **Persistence.** `/var/lib/docker` is the conversation's own claim, `sandbox-docker-<conversation id>`: images, build cache and named volumes survive a new pod and are deleted with the conversation. `/workspace` is another, `sandbox-workspace-<conversation id>` (SME-32): the conversation's files, checkouts and uncommitted work are there again in its next pod. At startup smelt deletes claims whose conversation no longer exists. `create_pod` waits for the conversation's previous pod to be gone first, so two pods never share a claim.
- **Networking like a Linux host.** A published port is at `localhost:<port>`, and any container port at the container's address. Docker's networks come from `172.20.0.0/14`, clear of k3s's ranges and the homelab LAN. The model's browser (`webfetch`, browsing sessions) reaches container addresses in that range, and `sandbox_preview_url`'s `host` gives the user a preview of one (`http://172-21-0-2-3000-42.preview.localhost:8181`). The sandbox agent relays those connections from `127.0.0.1:8089` and refuses any address outside the range; every other private address stays refused.

**What this gives up.** The sidecar is privileged and the sandbox holds its socket, so anything the model runs can become root on the node (`docker run --privileged -v /:/host …`). The sandbox still keeps a runaway command out of smelt's own process, but not off the node or away from other pods on it. With previews and no login yet, anyone who can reach smelt can reach root on that node. Not covered: a container started with `--network host` shares the pod's network and can reach the sandbox agent on `127.0.0.1:8088`, as it could on a Linux host. Running Docker unprivileged, in a user-namespaced pod, needs a newer node kernel ([SME-47](https://linear.app/smelt-agent/issue/SME-47)).

## Git in the sandbox

SME-32. The model clones with the `clone_repo` tool, and the user with "Work on a repo" in a new conversation, which starts the sandbox if needed. Either way the repo is checked out under `/workspace` and remembered for the conversation.

- **Keys and identity.** The Git page (`/git`) generates ed25519 SSH keys or imports an unencrypted OpenSSH private key, and sets the commit name and email. Add a key's public half to the git host (on GitHub, a deploy key with write access, or an account SSH key). Keys are stored unencrypted in Postgres for now (SME-48). Every pod gets them in `/etc/smelt` when it starts, and a change reaches running pods at once. The image points ssh (`/etc/ssh/ssh_config.d/smelt.conf`) and git (`/etc/gitconfig`) at those files, so the sandbox user's home directory is left alone, and a user's own `~/.ssh/config` or `~/.gitconfig` still wins. The image carries the published host keys of GitHub, GitLab and Bitbucket; other hosts are accepted on first use.
- **Rebuild the image.** The sandbox image needs `openssh-client` and those files: run `scripts/build-sandbox-image.sh` after pulling this change, or pods fail to start with a git setup error.
- **AGENTS.md.** When a repo is cloned, its top-level `AGENTS.md` (the first 32 KiB, Codex's default) goes into every turn's system prompt, under "Project instructions", with the repo's nested `AGENTS.md` files listed. The context detail view shows what was loaded and where it came from.
- **Trust.** A repo's `AGENTS.md` is only loaded once the user trusts its remote. The first clone of an unknown remote that has one puts a card in the chat showing the file, with Trust / Don't trust. The answer is remembered per remote (`git::remote_key`, e.g. `github.com/owner/repo`, whatever the URL form) and applies to every conversation's checkout of it. A repo opened with "Work on a repo" counts as trusted. `/git` lists the decisions; Forget asks again next time.
- **Changes.** After each turn smelt re-reads each checkout's `AGENTS.md`. A change shows in the sandbox panel with a Reload button; nothing reloads by itself.

## Pod metrics

The pods page (`/pods`) shows each sandbox pod's live memory and CPU use from the cluster's metrics API (metrics-server). smelt's service account needs `get`/`list` on `pods` in the `metrics.k8s.io` group for that; `k8s/smelt-park-rbac.yaml` grants it in both namespaces.
- **The local k3s** picks it up when the `k3s-bootstrap` compose service runs, so run `docker compose up` (or `docker compose run --rm k3s-bootstrap`) from the host after pulling the change.
- **Another cluster** needs the manifest applied there.

Until then, usage shows as "unavailable" and everything else on the page works.

## Dev over HTTPS (HTTP/2)

Over plain HTTP/1.1 (`http://localhost:8180`) a browser allows 6 connections per host, shared by every tab. Each smelt tab holds one open event stream (plus one more while the browsing panel is open), and ordinary requests need a free one too. So with about five smelt tabs open, a new tab can hang while loading, or a click waits. Over HTTP/2 the browser multiplexes up to 100 streams on one connection, and browsers only speak HTTP/2 over TLS. Production (homelab's TLS front end for `*.constantlee.us`) already negotiates HTTP/2.

For dev, the compose stack's `caddy` service serves the dev server at **`https://localhost:8443`** (`docker/caddy/Caddyfile`), with a certificate from Caddy's own local CA (`tls internal`). Everything else is unchanged; `http://localhost:8180` still works, and the browser tests use it.

**Trust Caddy's root certificate once**, so the browser accepts the certificate without a warning:
1. Find it in the `caddy-data` volume, in Caddy's data directory under `pki/authorities/local/`:
   ```bash
   docker compose exec caddy ls /data/caddy/pki/authorities/local/
   docker compose cp caddy:/data/caddy/pki/authorities/local/root.crt ./caddy-root.crt
   ```
   (Caddy's docs name the directory but not the file; check the `ls` output if `root.crt` isn't there.)
2. Import `caddy-root.crt` into your browser's certificate authorities (in Chrome: Settings → Privacy and security → Security → Manage certificates → Authorities → Import, trusting it for websites).

Accepting the browser's warning once also works, but the warning comes back whenever the browser forgets the exception.

**Check it's HTTP/2:** open `https://localhost:8443`, then DevTools → Network, add the "Protocol" column (right-click a column header). smelt's requests should say `h2`.

**MCP OAuth** redirects are built from `SMELT_BASE_URL` (`http://localhost:8180/` in the compose file). When using the HTTPS address for an OAuth flow, set it to `https://localhost:8443` (no trailing slash, per the table above).
