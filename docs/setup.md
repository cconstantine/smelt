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
# sidecar runs (SME-33), the same way. The image is named after the agent's
# sources, `smelt-sandbox:src-<hash>` (scripts/sandbox-image-ref, SME-102):
# a server with no SANDBOX_IMAGE runs the one named after the sources it was
# built from, and so do scripts/check.sh and the browser tier (SME-121).
# Run it again after pulling a change to the agent's sources,
# docker/sandbox/Dockerfile, Cargo.toml or Cargo.lock if no gate has built
# that image yet: until then `create_pod`
# fails within seconds with `ErrImageNeverPull`, naming the image and this
# command. (`--latest` also tags it `smelt-sandbox:latest`, for a server
# whose SANDBOX_IMAGE names that tag; nothing uses it by default.)
scripts/build-sandbox-image.sh

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
`scripts/clean-test-namespace.sh` (deletes every pod and Docker data claim in the
`smelt-park-test` namespace at once, a live test run's too, so only when none is going;
the tests' own harness already deletes anything there over an hour old, SME-134).

Several smelt servers can share a cluster's `smelt-park` namespace (the dev
server and a check server on a scratch database): each database labels its
pods and claims `smelt/instance=<its id>` and touches only its own (SME-115).
Don't run a copy of a database against the same cluster as its original: the
copy has the same instance id, so the two would delete and reuse each other's
sandboxes.

## CI

`.github/workflows/ci.yml` runs on every pull request, every push to `main`,
and on manual dispatch: the full `cargo test
--features server` suite (Postgres + real-cluster k3s sandbox tests), the
WASM `cargo check`, and the automated browser tier — the same tests and the
same `docker-compose.yml` stack described above and in
[testing.md](testing.md), just running on GitHub's runner instead of a local
machine. See [development-process.md](development-process.md#definition-of-done).

## Deploying

Running smelt for real, outside the dev stack. The [README](../README.md#deploying) has a summary; this is the full guide.

smelt is one server binary plus a web bundle. Alongside it, it needs:

- **Postgres.** smelt applies its own migrations at startup.
- **A single-node k3s cluster** for the sandboxes, with a default StorageClass (k3s ships `local-path`). It has to be k3s on one node: the sandbox image is imported straight into that node's containerd at k3s's socket path, with no registry, and pods never pull it. smelt runs the sandboxes in the `smelt-park` namespace under a `park` service account, and [k8s/smelt-park-rbac.yaml](../k8s/smelt-park-rbac.yaml) creates both. The cluster must allow privileged pods, because each sandbox's Docker sidecar is privileged. Live memory and CPU on the Sandboxes page also need metrics-server. The cluster needs to pull from Docker Hub: the image import's loader pod (`rancher/k3s`, pinned to k3s v1.34, whose `ctr` should match your cluster's containerd) and language-server pods both come from there.
- **The sandbox image**, delivered straight to the node's containerd with no registry involved.
- **Headless Chrome**, for `webfetch` and browsing sessions.

### Steps

1. **Get a build environment.** Build inside the repo's [Dockerfile](../Dockerfile) image (its `base` stage, on `rust:1.96-trixie`), or on Debian trixie on x86_64 with the same tools: Rust, the `wasm32-unknown-unknown` target and the Dioxus CLI at the version the Dockerfile pins (`dioxus-cli@0.7.9`; a different `dx` refuses to build the project). Other hosts don't work. The sandbox agent is linked against the build host's glibc and runs in a `debian:trixie-slim` image, so a newer glibc stops every sandbox from starting. `scripts/browser-check/setup.sh` also fetches Chrome's libraries with `apt-get`. Run the server binary in a matching environment too.
2. **Set up the cluster.** Run `kubectl apply -f k8s/smelt-park-rbac.yaml`, then make a kubeconfig for the `park` service account. [scripts/k3s-bootstrap.sh](../scripts/k3s-bootstrap.sh) shows how: it mints a long-lived token secret and writes the kubeconfig. It's written for the compose stack, so use your cluster's API address, an admin kubeconfig and your own paths instead of its `k3s:6443`, `/k3s-admin/k3s.yaml`, `/k8s/` and `/out/`. Point `KUBECONFIG` at that file.
3. **Deliver the sandbox image.** With `DOCKER_HOST` and `KUBECONFIG` set, run `scripts/build-sandbox-image.sh` from the same tree as the server you build in step 5. Run it again after every upgrade of smelt. The image is named after the agent's sources (`smelt-sandbox:src-<hash>`), and a server with no `SANDBOX_IMAGE` runs the one named after the sources it was built from (SME-121), so it can't start pods from an older agent. If the image isn't on the node, `create_pod` fails within seconds with `ErrImageNeverPull`, naming the image and this command.
4. **Install headless Chrome:** `scripts/browser-check/setup.sh`. Then set `BROWSER_CHECK_CACHE` to the absolute path of the `.browser-check-cache` directory it creates.
5. **Build:** `dx bundle --platform web`. It produces a release server binary next to its web bundle, and dx's output says where. Run the binary from that layout: it serves the bundle from the `public/` directory beside it.
6. **Configure and start it.** Set the environment variables in [docs/setup.md](#environment-variables). The ones a deployment needs are:
   - `DATABASE_URL` and `KUBECONFIG`.
   - `PORT`: default `8080`.
   - `SMELT_BASE_URL`: the public address, used for MCP OAuth redirects and preview pages.
   - `SMELT_ALLOWED_HOSTS`: the host names smelt is reached by. Requests for any other host name are then refused. IP addresses and `localhost` always work, so this is no substitute for the firewall below.
   - `SMELT_PREVIEW_URL` and `SMELT_PREVIEW_ADDR`: where sandbox previews live.
   - `BROWSER_CHECK_CACHE`: from step 4.
7. **Put it behind TLS.** Use a reverse proxy that speaks HTTP/2: each tab holds an open event stream, and HTTP/1.1 allows only six connections per host. Route the preview host names (for example `{port}-{conversation}-smelt.example.com`) to `SMELT_PREVIEW_ADDR`'s port. They need wildcard DNS and a wildcard TLS certificate. The proxy must pass the browser's `Host` header through unchanged (in nginx, `proxy_set_header Host $host;`), since the preview listener reads the conversation and port from it, and must pass WebSocket upgrades through for a dev server's live reload. See [Sandbox previews](#sandbox-previews).
8. **Add a model provider.** Open smelt, go to **Model providers** in the sidebar, and add one with its key. Nothing about the model is read from the environment.

### Before you expose it

smelt has **no login**. Anyone who can reach it can use your model keys and run commands in your sandboxes. And because the Docker sidecar is privileged, those commands can reach root on the sandbox's node. Keep smelt behind something that authenticates, such as a VPN or an authenticating proxy, and the preview host names with it. And make that the only way in: smelt listens on every interface (`0.0.0.0:$PORT`, and previews on `0.0.0.0:8181` by default). So firewall `PORT`, and either firewall the preview port or set `SMELT_PREVIEW_ADDR=127.0.0.1:8181`. [docs/setup.md](#docker-in-the-sandbox) has the details. Model provider keys, MCP servers' headers, OAuth tokens and client secrets, and the git SSH private key are stored in plain text in Postgres for now, so protect the database and its backups the same way.

The model is a risk too, not only outsiders. It runs whatever it decides to in its sandbox, and a web page, repo or tool result it reads can talk it into something. Anything it runs can become root on the k3s node. Sandboxes have unrestricted network access (smelt's private-address guard covers only its own `webfetch` and `http_request`, not a command in a terminal). And every sandbox can read the git SSH private key. So give the k3s cluster a machine or VM of its own, away from smelt's database and anything else you care about. And use a deploy key or a low-privilege account's key, not your main account's.

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
| `RUST_LOG` | no | errors only | Standard `tracing-subscriber` env filter, e.g. `RUST_LOG=info,tower_http=debug`. The `chromiumoxide::conn` and `chromiumoxide::handler` targets are always off, even with `RUST_LOG` set: they log a harmless deserialize error on every real page (`log_filter_directives` in `src/main.rs`). `scripts/check-server` defaults it to `info` (see [testing.md](testing.md)). Only the console: the trace export has its own fixed filter (see [Tracing](#tracing-opentelemetry)). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | no | unset: no tracing export | Where smelt sends its trace spans over OTLP/HTTP (`/v1/traces` is added), e.g. `http://tempo:4318`, which the compose `smelt` service sets. Unset or empty, no exporter is built and nothing is sent (SME-137). Read once at startup. The SDK's other `OTEL_*` variables apply too (`OTEL_TRACES_SAMPLER`, `OTEL_BSP_*`, `OTEL_EXPORTER_OTLP_HEADERS`). |
| `OTEL_SERVICE_NAME` | no | `smelt` | The exported traces' `service.name`. |
| `KUBECONFIG` | yes, for sandbox code/tests | — | Read by `kube::Client::try_default()` (`src/sandbox/manager.rs`). In `docker-compose.yml`, `smelt`'s `KUBECONFIG` points at the kubeconfig `k3s-bootstrap` generates for the `park` service account against the compose-provided `k3s` service — a hermetic test cluster, not a real deployment target. Point it at `.kubeconfig.yaml` (gitignored) instead to deliberately target the real `homelab` cluster. See [SME-7](https://linear.app/smelt-agent/issue/SME-7). |
| `SANDBOX_MEMORY_LIMIT` | no | `8Gi` | Default memory limit for a sandbox pod's container — a plain Kubernetes quantity string. Just the *default*: `create_pod`'s `memory_limit` parameter overrides it per pod, up to the `smelt-park`/`smelt-park-test` namespace's `LimitRange` ceiling (`k8s/smelt-park-rbac.yaml`). Hitting the limit kills the whole pod at once (`memory.oom.group=1` on this cluster), not just the offending process — see [SME-12](https://linear.app/smelt-agent/issue/SME-12). The pod asks for no memory up front (`requests.memory: 0`, SME-77): without a request Kubernetes copies the limit into one, so every idle pod reserved its whole limit. Under node memory pressure that makes sandbox pods the first evicted. Sandbox pods have no CPU request or limit at all. |
| `SANDBOX_IMAGE` | no | `docker.io/library/smelt-sandbox:src-<hash>`, named after the agent sources the server was built from | The sandbox pod's image reference — `docker/sandbox/Dockerfile`, built and delivered with no registry involved by `scripts/build-sandbox-image.sh`. Must match whatever `ctr images import` actually registered the image as, not an arbitrary tag — see SME-17. The default is the image `scripts/sandbox-image-ref` prints for the tree the server was built from (`build.rs` computes the same hash, SME-121), so a server never runs an older agent than its own without saying so: a node without that image fails `create_pod` with `ErrImageNeverPull` and the command that builds it. `scripts/check.sh`, `scripts/browser-tier`, `scripts/check-server` and CI set it to the working tree's own image too (SME-102). Set it only to run another image, such as `smelt-sandbox:latest` (`build-sandbox-image.sh --latest` moves that tag). |
| `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS` | no | `90` | How long `wait_for_running` (`src/sandbox/manager.rs`) waits for a pod to reach `Running`. Then it takes up to 10 s more to read the pod and its events again: a pod `Running` by then has started (with a `warn`), and any other gives up with `SandboxError::Timeout`, which says what the pod is stuck on (SME-132): its first `False` condition with the pod's own reason (e.g. `PodScheduled: Unschedulable: persistentvolumeclaim … not found`) and how long it has been there, the pod's age, the stages it passed as offsets from creation, each container's state, and up to 5 of its events, `Warning`s first (`FailedMount`, `FailedCreatePodSandBox`, `Unhealthy`). A start that succeeds after more than half this timeout logs a `warn` with the same timeline, so a server's log shows how close real starts come to the limit. A container stuck in a state that won't recover (an image pull failure, a crash loop) fails at once with `SandboxError::StartFailed` instead of waiting out the timeout. The default covers the Docker sidecar's 60 s startup probe plus claim provisioning (SME-62 B15); a CPU-constrained CI runner schedules pods measurably slower, so `.github/workflows/ci.yml` raises this for its `cargo test --features server` run. |
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

## Tracing (OpenTelemetry)

smelt's spans (each request, turn, model call, tool call, MCP call and sandbox step; see [architecture.md](architecture.md#tracing-opentelemetry)) go to **Tempo**, and you read them in **Grafana**, both in the compose stack (SME-137):

```bash
docker compose up -d tempo grafana   # neither starts with the rest of the stack
```

- **Grafana** is at http://localhost:8182, with Tempo as its data source. There's no login: you're an anonymous Editor, which can explore but not add data sources. It listens on localhost only and answers only to the name `localhost` (any other Host is redirected there), so another site's page can't reach it by DNS rebinding. Explore → Tempo → TraceQL, e.g. `{ name = "turn" }` for turns, `{ span.conversation_id = 12 }` for one conversation's, `{ status = error }` for failures.
- **Tempo** has no host port; smelt reaches it as `tempo:4318`, which the `smelt` service's `OTEL_EXPORTER_OTLP_ENDPOINT` names. Traces are kept a week, in the `tempo-data` volume (`docker/tempo/tempo.yaml`).
- **Check servers** in the container export too (they inherit the variable). Tell them from the dev server by the resource's `smelt.database`: the dev server's is `smelt`, a scratch check server's `smelt_scratch_…`, e.g. `{ resource.smelt.database = "smelt" }`. (`smelt.port` is the server's own port; under `dx serve` that's one `dx` picks, not 8080.)
- **Spans export in batches**, about every 5 s, so a trace shows up a few seconds after its turn ends. A span still open when smelt stops (a running turn) is lost.
- **With Tempo down**, smelt runs as before and logs one `opentelemetry_sdk` error per failed batch. To turn tracing off, unset `OTEL_EXPORTER_OTLP_ENDPOINT` (e.g. `OTEL_EXPORTER_OTLP_ENDPOINT= dx serve ...`).
- **After pulling this change**, recreate the `smelt` container to pick up the variable (`docker compose up -d` does; it ends a running `dx serve`).

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
- **Rebuild the image.** The sandbox image needs `openssh-client` and those files: run `scripts/build-sandbox-image.sh` after pulling this change, or pods fail to start with a git setup error.
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
