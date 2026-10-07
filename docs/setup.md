# Setup & Running

## Commands

```bash
# ── Dev with hot reload (single command) ───────────────────────────────────
dx serve --fullstack
# Fullstack mode runs the real Axum server (SSR + server functions) and
# rebuilds/hydrates the WASM client on change, all through one process that
# dx manages — unlike a CSR-only app, there's no separate hand-rolled server
# process or web-mode proxy to wire up.
# dx serves its own dev address (check its startup log; open that in a
# browser), sets PORT for the server binary it launches, and proxies to it.
# In the dev container, use the address compose publishes instead:
#   dx serve --fullstack --addr 0.0.0.0 --port 8080
# (the container's :8080 is the host's :8180; see docker-compose.yml).

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
# sidecar runs (SME-33), the same way. `--latest` makes it the image a
# server with no SANDBOX_IMAGE runs (`smelt-sandbox:latest`); without it,
# only `smelt-sandbox:src-<hash of the agent sources>` is imported, the one
# scripts/check.sh and the browser tier test against (SME-102). Run it with
# `--latest` again after pulling a change to the agent.
scripts/build-sandbox-image.sh --latest

# ── Set up chrome-headless-shell, for real webfetch/browsing usage ─────────
# The `webfetch` tool, and the persistent browsing-session tools
# (`open_browser_session`/`browser_navigate`/... plus the live panel) built
# on the same shared browser, all navigate a real headless browser
# (`chromiumoxide`, driving `chrome-headless-shell` over CDP) — not just
# `src/browser_tests.rs`'s own test tier anymore. `scripts/browser-check/setup.sh`
# downloads the binary plus its missing shared libraries into
# `.browser-check-cache/` (gitignored) — no root needed, safe to re-run.
# `BROWSER_CHECK_CACHE` moves that directory, for setup.sh and for smelt
# alike: smelt looks for Chrome there, or, when it's unset, at
# `<checkout it was built from>/.browser-check-cache/` (the compile-time
# `CARGO_MANIFEST_DIR`, `src/headless_chrome.rs`). So a binary running
# anywhere but its build checkout (a deployed build, another worktree) needs
# it set; `scripts/check-server` and `scripts/browser-tier` set it to the
# main checkout's cache.
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

Helper scripts: `scripts/check.sh` (the checks every commit must pass: both
builds with no warnings, plus the server tests), `scripts/check-server`
(`dx serve` from a separate worktree, for hands-on checks), and
`scripts/clean-test-namespace.sh` (deletes pods and Docker data claims left behind
in the `smelt-park-test` namespace).

## CI

`.github/workflows/ci.yml` runs on every pull request, every push to `main`,
and on manual dispatch: the full `cargo test
--features server` suite (Postgres + real-cluster k3s sandbox tests), the
WASM `cargo check`, and the automated browser tier — the same tests and the
same `docker-compose.yml` stack described above and in
[testing.md](testing.md), just running on GitHub's runner instead of a local
machine. See [development-process.md](development-process.md#definition-of-done).

## Model providers

smelt doesn't read a model or an API key from the environment (SME-72). Set them up in the app: **Model providers** in the sidebar (`/providers`). Until there's one, a conversation says "No model provider is set up" above its message box, with a link there, and Send waits.

- **A provider** is a name, a kind, a base URL and a key. Every turn goes to the base URL's `/v1/messages`, whatever the kind. The key is sent as `x-api-key` (API key) or `Authorization: Bearer` (bearer token), and pages only ever show its last four characters. Changing a provider's base URL needs the key entered again, so a stored key only goes where it was entered for. It's stored in plain text for now, like the SSH key ([SME-48](https://linear.app/smelt-agent/issue/SME-48)).
- **The kind** decides how smelt lists the provider's models and what it learns about each (`src/anthropic/models.rs`):
  - **Anthropic** lists `GET /v1/models`, whose entries carry each model's context window (`max_input_tokens`) and thinking support.
  - **Ollama** lists `GET /api/tags`, then asks `POST /api/show` about each model: whether it can call tools and think, and a Modelfile's `num_ctx`. Without `num_ctx`, the window the model is loaded with right now (`GET /api/ps`), if it's loaded. Ollama's own default window (4k, 32k or 256k by GPU memory, or `OLLAMA_CONTEXT_LENGTH`) isn't reported anywhere, so for a model with neither, set the window by hand. Local Ollama needs a key but ignores it; any value works.
  - **llama.cpp** (`llama-server`, SME-111) lists `GET /v1/models` and, on the provider's page or when a model is picked, reads `GET /props`: each slot's context window (`default_generation_settings.n_ctx`, applied to the model the server serves), its slot count and build, and the chat template's capabilities (`chat_template_caps`), kept on the provider and shown under its Models heading. Its turns tell the template, through `chat_template_kwargs`, the model's Effort (as `reasoning_effort`, only when the template supports it: template's default, low, medium or high), `enable_thinking: false` when thinking is off (compaction always sends it), and the provider's **Keep earlier reasoning** setting (as `preserve_thinking`, only when the template supports it). The template decides what each effort means; a value it refuses fails the turn with the template's error. The form starts it with a bearer token and prompt caching off. A new provider's address, or an Other provider's, that answers `/props` like llama.cpp gets a "This looks like a llama.cpp server" suggestion; the form never switches by itself. That check sends no key.
  - **Other** (any Anthropic-compatible server, such as a gateway) tries `GET /v1/models`, reading a context window from `max_input_tokens` or llama.cpp's `meta.n_ctx`.
- **The default model** is what a conversation without a model of its own takes when its next turn starts, and then keeps. That covers every conversation from before providers existed. Each conversation's model can be changed from the picker above its message box; the change applies from its next turn.
- **Per-model settings**, on the provider's page: a context window (smelt compacts a conversation before it outgrows it, and the context indicator measures against it), **Max reply tokens** (the most one reply may be, thinking included; unset, what the provider reports, which only Anthropic's listing does, else 16,384 on an Anthropic or Other provider and no cap but the window on Ollama or llama.cpp once the window is known; on Anthropic it can't go above the listed cap; see [api.md](api.md) for how a turn's reply budget follows the room left, SME-111), on a llama.cpp provider a **Reasoning budget** (SME-111: a turn sends `thinking: {"type": "enabled", "budget_tokens": N}`, and llama.cpp forces the end of thinking at N tokens so the model answers; automatic is three quarters of the reply budget, leaving at least 4,096 tokens to answer, and a number here caps it; the other kinds keep adaptive thinking, since Anthropic refuses a budget on current models, Ollama doesn't enforce one, and Other may be a gateway in front of Claude) and thinking on or off. Unset, they come from what the provider reported, else a known `claude-*` model's window, else 200,000, shown as "unknown, assuming 200,000". Thinking defaults to on unless the provider says the model can't. `run_turn` still retries without thinking if a request fails with Ollama's "error parsing tool call" (see [api.md](api.md)).

## Environment Variables

Copy `.env.example` to `.env` if you need any of these. Loaded automatically at server startup
(`dotenvy::dotenv()` in `main.rs`); a value already set in the real
environment takes precedence over `.env`. Most vars treat set-but-empty the
same as unset. The exception: an empty `DATABASE_URL` fails to parse and
panics at startup just like an unset one.


| Variable | Required | Default | Notes |
|---|---|---|---|
| `DATABASE_URL` | yes | — | Postgres connection string; `db::init()` panics on startup if unset or empty. Set in `docker-compose.yml`'s `smelt` service, pointing at the `postgres` compose service (only reachable from other compose services, not the host) — only needed in `.env` if running outside docker compose. |
| `PORT` | no | `8080` | Port the server binary binds, on `0.0.0.0` (`src/main.rs`). Under `dx serve`, dx sets it for the server it launches and proxies to that. |
| `RUST_LOG` | no | errors only | Standard `tracing-subscriber` env filter, e.g. `RUST_LOG=info,tower_http=debug`. The `chromiumoxide::conn` and `chromiumoxide::handler` targets are always off, even with `RUST_LOG` set: they log a harmless deserialize error on every real page (`log_filter_directives` in `src/main.rs`). |
| `KUBECONFIG` | yes, for sandbox code/tests | — | Read by `kube::Client::try_default()` (`src/sandbox/manager.rs`). In `docker-compose.yml`, `smelt`'s `KUBECONFIG` points at the kubeconfig `k3s-bootstrap` generates for the `park` service account against the compose-provided `k3s` service — a hermetic test cluster, not a real deployment target. Point it at `.kubeconfig.yaml` (gitignored) instead to deliberately target the real `homelab` cluster. See [SME-7](https://linear.app/smelt-agent/issue/SME-7). |
| `SANDBOX_MEMORY_LIMIT` | no | `8Gi` | Default memory limit for a sandbox pod's container — a plain Kubernetes quantity string. Just the *default*: `create_pod`'s `memory_limit` parameter overrides it per pod, up to the `smelt-park`/`smelt-park-test` namespace's `LimitRange` ceiling (`k8s/smelt-park-rbac.yaml`). Hitting the limit kills the whole pod at once (`memory.oom.group=1` on this cluster), not just the offending process — see [SME-12](https://linear.app/smelt-agent/issue/SME-12). The pod asks for no memory up front (`requests.memory: 0`, SME-77): without a request Kubernetes copies the limit into one, so every idle pod reserved its whole limit. Under node memory pressure that makes sandbox pods the first evicted. Sandbox pods have no CPU request or limit at all. |
| `SANDBOX_IMAGE` | no | `docker.io/library/smelt-sandbox:latest` | The sandbox pod's image reference — `docker/sandbox/Dockerfile`, built and delivered with no registry involved by `scripts/build-sandbox-image.sh` (`:latest` only with `--latest`). Must match whatever `ctr images import` actually registered the image as, not an arbitrary tag — see SME-17. `scripts/check.sh`, `scripts/browser-tier`, `scripts/check-server` and CI set it to `docker.io/library/smelt-sandbox:src-<hash>`, the image built from the working tree's own agent sources (`scripts/sandbox-image-ref`, SME-102), so a branch that changes the agent tests its own image without replacing the one a dev server runs. |
| `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` | no | `90` | How long `wait_for_running` (`src/sandbox/manager.rs`) waits for a pod to reach `Running` before giving up with `SandboxError::Timeout`, which carries the pod's own reason for still being pending (e.g. `PodScheduled: Unschedulable: persistentvolumeclaim … not found`) when it has one. A container stuck in a state that won't recover (an image pull failure, a crash loop) fails at once with `SandboxError::StartFailed` instead of waiting out the timeout. The default covers the Docker sidecar's 60 s startup probe plus claim provisioning (SME-62 B15); a CPU-constrained CI runner schedules pods measurably slower, so `.github/workflows/ci.yml` raises this for its `cargo test --features server` run. |
| `SMELT_BASE_URL` | no | derived from the request | Overrides the scheme+host `src/mcp_oauth.rs` builds an MCP OAuth redirect_uri from (`/mcp-servers`' Connect flow), and is where a preview's "sandbox isn't running" page links back to (see [Sandbox previews](#sandbox-previews)). Without it, the base URL is derived from the incoming request's `Host` header (`X-Forwarded-Proto` for scheme) — wrong if smelt sits behind a proxy/tunnel that doesn't forward a `Host` a browser/OAuth provider could actually reach. No trailing slash. Set-but-empty is treated as unset. Once `SMELT_ALLOWED_HOSTS` is set, its host is accepted too. |
| `SMELT_ALLOWED_HOSTS` | no | (unset) | Comma-separated host names smelt is reached by (e.g. `smelt.example.com`); `SMELT_BASE_URL`'s host is added. Once set, a request whose `Host` is any other name is refused, which stops DNS rebinding (a page whose name later resolves to smelt's address). Behind a proxy, list the name the proxy sends as `Host`, not only the public one. IP addresses and `localhost` names always work. Unset, any name is served and startup logs a warning. Separately, a write (anything but GET/HEAD/OPTIONS) that a browser says came from another site, a sandbox preview included, is always refused (`src/request_guard.rs`, SME-51). |
| `SANDBOX_DOCKER_MEMORY_LIMIT` | no | `8Gi` | Memory limit for each sandbox pod's Docker sidecar (SME-33). Every container Docker runs counts against it, not against `SANDBOX_MEMORY_LIMIT`; an OOM kill restarts only the sidecar. `create_pod`'s `docker_memory_limit` overrides it per pod, up to the same `LimitRange` ceiling. See [Docker in the sandbox](#docker-in-the-sandbox). |
| `SANDBOX_DOCKER_STORAGE_SIZE` | no | `20Gi` | Size each conversation's Docker data claim (`sandbox-docker-<conversation id>`, its images, build cache and named volumes) requests. |
| `SANDBOX_WORKSPACE_STORAGE_SIZE` | no | `20Gi` | Size each conversation's `/workspace` claim (`sandbox-workspace-<conversation id>`, its files and checkouts) requests. |
| `SANDBOX_VOLUME_STORAGE_SIZE` | no | `10Gi` | Size each generic volume's claim (`sandbox-volume-<id>`, the volumes configured on the `/sandbox-volumes` page) requests (`src/sandbox/spec.rs`). Not configurable per volume. |
| `SANDBOX_DOCKER_IMAGE` | no | `docker.io/library/docker:29-dind` | The Docker sidecar's image, delivered into the node by `scripts/build-sandbox-image.sh`. Only its `dockerd` is used, never its entrypoint. |
| `BROWSER_CHECK_CACHE` | no | `<build checkout>/.browser-check-cache` | Where headless Chrome and its libraries are: `scripts/browser-check/setup.sh` downloads into it and smelt launches Chrome from it, for `webfetch` and browsing sessions (see [Commands](#commands) above). Set it, as an absolute path, wherever smelt runs from somewhere other than the checkout it was built in: smelt reads a relative one against its own working directory. |
| `SMELT_MASON_REGISTRY_URL` | no | `https://raw.githubusercontent.com/mason-org/mason-registry/main` | Where the Language servers page's lookup reads mason's `packages/<name>/package.yaml` (see [Language servers](#language-servers)). |
| `SMELT_HELIX_LANGUAGES_URL` | no | `https://raw.githubusercontent.com/helix-editor/helix/master/languages.toml` | Helix's `languages.toml`, for the lookup's arguments, file types, root markers and settings. |
| `SMELT_PRICE_CATALOG_URL` | no | `https://models.dev/api.json` | Where model prices come from (SME-106, `src/pricing.rs`): models.dev's catalog, fetched at startup and every hour, with the last good copy kept in the `price_catalog` table so a restart without network still has prices. A failed fetch keeps the last copy and logs a warning. Each provider's "Prices from" setting names its entry; a call's cost is fixed when it's recorded. Tests point it at a stand-in. |
| `SMELT_PREVIEW_URL` | no | `http://{port}-{conversation}.preview.localhost:8181` | The address a sandbox preview gets (see [Sandbox previews](#sandbox-previews)): a scheme and host holding `{port}` and `{conversation}` once each, with something other than digits between them, and no path. In production, e.g. `https://{port}-{conversation}-smelt.constantlee.us`. An invalid value turns previews off, logged as an error at startup; the rest of smelt runs as usual. |
| `SMELT_PREVIEW_ADDR` | no | `0.0.0.0:8181` | Where the preview proxy listens. It's a listener of its own, not a route on smelt's main port (see below). A port that can't be bound turns previews off, logged as an error. |

## Built-in MCP servers

At startup, smelt adds any built-in MCP server that isn't configured yet, matched by name (`mcp::ensure_default_servers`). Today that's one: **`exa`**, Exa's hosted MCP server at `https://mcp.exa.ai/mcp?tools=web_search_exa`, which gives the model web search (`mcp__exa__web_search_exa`) with no account or key. The `tools=` parameter limits it to search, so reading a page stays with `webfetch`/`http_request`.

- **Keyless by default.** Exa's free, rate-limited mode. Its limits aren't published; going over them comes back as a tool error. To lift them, add an `x-api-key` header with an Exa key to the entry on `/mcp-servers`.
- **Edits are kept.** Only the name is matched, so a changed URL or an added header survives restarts.
- **Deleting doesn't stick.** A deleted entry comes back on the next start. To turn search off, change its URL or tool list instead.
- **Search queries go to Exa**, unauthenticated in keyless mode.

## Sandbox previews

A server running in a conversation's sandbox pod (a dev server, a web app) can be opened in two browsers, both through a Kubernetes port-forward to the pod, so a server bound to `127.0.0.1` inside the pod works too (SME-42):

- **The model's own browser.** In `webfetch` and browsing sessions, `http://localhost:<port>/` (also `127.0.0.1` and `[::1]`) is that port in the conversation's own pod. Every other loopback or private address is still refused.
- **The user's browser.** The model calls `sandbox_preview_url` with the port; the sandbox panel then shows an "Open preview · port N" link to `SMELT_PREVIEW_URL` for that port and conversation, e.g. `http://5173-42.preview.localhost:8181`.

A preview link whose conversation has no running sandbox (stopped, or the conversation deleted) shows a page saying so and how to get the preview back, with a link to the conversation built from `SMELT_BASE_URL`; without it, the page names the conversation instead of linking (SME-46).

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

- **Key and identity.** The Git page (`/git`) generates an ed25519 SSH key or imports an unencrypted OpenSSH private key, and sets the commit name and email. There's one key at a time for now: with several, the git host takes the first it knows, which breaks per-repo deploy keys. Add its public half to the git host as an account SSH key (on GitHub, Settings > SSH and GPG keys), so it reaches every repo. Keys are stored unencrypted in Postgres for now (SME-48). Every pod gets them in `/etc/smelt` when it starts, and a change reaches running pods at once. The image points ssh (`/etc/ssh/ssh_config.d/smelt.conf`) and git (`/etc/gitconfig`) at those files, so the sandbox user's home directory is left alone, and a user's own `~/.ssh/config` or `~/.gitconfig` still wins. The image carries the published host keys of GitHub, GitLab and Bitbucket; other hosts are accepted on first use.
- **Rebuild the image.** The sandbox image needs `openssh-client` and those files: run `scripts/build-sandbox-image.sh --latest` after pulling this change, or pods fail to start with a git setup error.
- **AGENTS.md.** Nothing loads by itself. A clone lists the checkout's `AGENTS.md` files (top-level and nested), which the model sees in the system prompt's repo list; it loads the ones it needs with the `load_instructions` tool, and a loaded file (the first 32 KiB, Codex's default) stays in every turn's system prompt under "Project instructions" until the model loads it again. The context detail view shows what's loaded and where it came from.
- **Trust.** Only a repo whose remote the user trusts loads. When the model asks for a file from a remote the user hasn't decided about, the chat shows that exact file on a Trust / Don't trust card, and Trust loads exactly it; other conversations' pending requests for the remote are dropped and those conversations told to ask again. The answer is remembered per remote (`git::remote_key`, e.g. `github.com/owner/repo`, whatever the URL form). A repo opened with "Work on a repo" counts as trusted. `/git` lists the decisions; Forget asks again next time.
- **Existing directories.** A clone goes straight into `/workspace/<dir>`, and git refuses a directory that already exists and isn't empty, so nothing there is overwritten. A clone that fails the ordinary way leaves nothing behind (git removes what it made); one cut off midway (Stop, the 10-minute timeout, a restart) can leave a partial checkout, and the retry's error says to delete it or clone into another directory.

## Language servers

SME-35. The user configures language servers on the Language servers page (`/language-servers`), one row each in `language_servers`: an image, an install command, the command to run, file types (extension to language id), root markers, initialization options and settings, and memory and CPU limits. "Look up" fills the form from mason's registry and Helix's `languages.toml` (`src/lsp/catalog.rs`); it's a suggestion to check, not an install.

- **Where they run.** `start_language_server` starts a server in a pod of its own, `lsp-<pod id>-<name>`, next to the conversation's sandbox pod: on the same node (the workspace claim is `ReadWriteOnce`), with `/workspace` mounted, and owned by the sandbox pod so Kubernetes deletes it with the sandbox. It runs as uid 1000 with `HOME=/tmp/home`; the install runs when the pod starts, so each new sandbox reinstalls (a few seconds for pyright, a download for rust-analyzer). smelt talks to the server over `pods/exec` stdin and stdout.
- **Images.** Server pods pull public images (`rust:1`, `node:22-slim`, `python:3-slim`, `golang:1`, `debian:trixie-slim`), so the cluster needs to reach Docker Hub, and an install command needs whatever network it downloads from.
- **Memory.** A server that runs past its memory limit takes only its own pod down; the model is told once, and the user can raise the limit on the page.
- **Changes.** Deleting, disabling or renaming a server stops its pods everywhere. Other changes apply when the model starts it again (it's told the settings changed).

## Pod metrics

The Sandboxes page (`/pods`) shows each sandbox pod's live memory and CPU use from the cluster's metrics API (metrics-server). smelt's service account needs `get`/`list` on `pods` in the `metrics.k8s.io` group for that; `k8s/smelt-park-rbac.yaml` grants it in both namespaces.
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
