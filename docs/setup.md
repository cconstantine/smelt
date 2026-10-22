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
# `src/browser_tests/`'s own test tier anymore. `scripts/browser-check/setup.sh`
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
# use #[sqlx::test], which needs a reachable DATABASE_URL. They run only under
# the cluster lock, which this takes (SME-212). See testing.md.
scripts/cluster-test --features server
```

**Run all of it inside the compose stack's `smelt` service rather than
installing a toolchain wherever you happen to be.** That service
(`docker-compose.yml`, built from `Dockerfile`'s `dev` target) *is* the
development environment — rust with the wasm32 target, `rustfmt` and `clippy`,
the pinned `dioxus-cli`, `sqlx-cli`, `gh`, the docker CLI and Playwright — and
compose hands it Postgres, a Docker sidecar and a ready kubeconfig for the
hermetic test cluster (`KUBECONFIG=/etc/smelt/k3s/park-kubeconfig.yaml`,
written by `k3s-bootstrap`). So the whole gate runs there with nothing
installed by hand, which is also what CI does, one step per command
([.github/workflows/ci.yml](<../.github/workflows/ci.yml>)):

```bash
docker compose up -d postgres docker k3s
docker compose run --rm smelt scripts/build-sandbox-image.sh  # once per change to the agent's sources
docker compose run --rm smelt scripts/check.sh
```

An agent working inside a smelt conversation sandbox — which has none of that
toolchain, no Postgres and no cluster — should reach for the same thing through
the sandbox's own Docker rather than `apt-get install`-ing rust, clippy and a
wasm target piecemeal: an installed toolchain costs more to get and proves
less, since it reaches neither the database nor a cluster. A daemon that
refuses privileged containers can't bring up the `k3s` and `docker` sidecars,
and then the cluster-backed tests fall to CI. What this seat is, in practice — one clone, the sequential gate, what is and isn't installed, how the run ends — is the "Working from a conversation sandbox" section of [the persona rules](personas/rules.md). This deployment's daemon allows privileged containers, so the real gate runs in a conversation sandbox and not only in CI.

Three things trip a conversation sandbox's stack (SME-257's retrospective, proposal 2):

- **Building the sandbox image needs memory.** The build runs in the pod's
  Docker sidecar, whose limit is `SANDBOX_DOCKER_MEMORY_LIMIT` (8 Gi by
  default) unless `create_pod`'s `docker_memory_limit` raises it, up to the
  namespace's `LimitRange` ceiling. One of SME-226's builds went through at 24 Gi;
  another was OOM-killed twice before 48 Gi let it through, and each kill showed
  in the build log as an apt or network error, not as memory. Create the pod with
  `docker_memory_limit: "48Gi"` when it will build the image.
- **Rebuild the sandbox image after a merge that changes the agent's
  sources** (a merge of `origin/main` into the branch included), before the
  next gate: the gate's pods run the image named after the sources
  (`scripts/sandbox-image-ref`), and until it is built `scripts/check.sh`
  stops at `scripts/cluster-doctor`, naming the missing image.
- **`k3s` won't start again after its container is recreated** when the node
  IP stored in its volume no longer matches the new container's: it exits
  quietly, and its log ends with `failed to find interface with specified
  node ip`. Drop the cluster's state and bootstrap it again:

  ```bash
  docker compose down
  p=${COMPOSE_PROJECT_NAME:-$(basename "$PWD")}  # compose's project name: the clone's directory
  docker volume rm "${p}_k3s-data" "${p}_k3s-config"
  docker compose up -d postgres docker k3s
  docker compose run --rm k3s-bootstrap
  ```

  The cluster then starts empty, so build the sandbox image again too.

Helper scripts: `scripts/check.sh` (the checks every commit must pass: both
builds with no warnings, plus the server tests), `scripts/check-server`
(`dx serve` from a separate worktree, for hands-on checks), and
`scripts/clean-test-namespace.sh` (deletes every pod and Docker data claim in the
`smelt-park-test` namespace at once; it takes the cluster lock, so it waits for a running test run to finish;
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
machine. See [process/done.md](process/done.md).

## Deploying

Running smelt for real, outside the dev stack. The [README](../README.md#deploying) has a summary; this is the full guide.

smelt is one server binary plus a web bundle. Alongside it, it needs:

- **Postgres.** smelt applies its own migrations at startup.
- **A single-node k3s cluster** for the sandboxes, with a default StorageClass (k3s ships `local-path`). It has to be k3s on one node: the sandbox image is imported straight into that node's containerd at k3s's socket path, with no registry, and pods never pull it. The node is x86_64 (`amd64`), and smelt pins every pod it makes to `kubernetes.io/arch: amd64`: every image the project publishes is built linux/amd64 only, so a node of another architecture could only fail the pull. smelt runs the sandboxes in the `smelt-park` namespace under a `park` service account, and [k8s/smelt-park-rbac.yaml](../k8s/smelt-park-rbac.yaml) creates both. The cluster must allow privileged pods, because each sandbox's Docker sidecar is privileged. Live memory and CPU on the Sandboxes page also need metrics-server. The cluster needs to pull from Docker Hub: the image import's loader pod (`rancher/k3s`, pinned to k3s v1.34, whose `ctr` should match your cluster's containerd) and language-server pods both come from there.
- **The sandbox image**, delivered straight to the node's containerd with no registry involved.
- **Headless Chrome**, for `webfetch` and browsing sessions.

The steps below build all of that yourself, which is the only route that has ever been tested end to end. [Deploying from published images](#deploying-from-published-images) covers the alternative: pull the server and sandbox images from GHCR instead, and skip the building.

### Steps

1. **Get a build environment.** Build inside the repo's [Dockerfile](../Dockerfile) image (its `base` stage, on `rust:1.96-trixie`), or on Debian trixie on x86_64 with the same tools: Rust, the `wasm32-unknown-unknown` target and the Dioxus CLI at the version the Dockerfile pins (`dioxus-cli@0.7.9`; a different `dx` refuses to build the project). Other hosts don't work. The sandbox agent is linked against the build host's glibc and runs in a `debian:trixie-slim` image, so a newer glibc stops every sandbox from starting. `scripts/browser-check/setup.sh` also fetches Chrome's libraries with `apt-get`. Run the server binary in a matching environment too.
2. **Set up the cluster.** Run `kubectl apply -f k8s/smelt-park-rbac.yaml`, then make a kubeconfig for the `park` service account. [scripts/k3s-bootstrap.sh](../scripts/k3s-bootstrap.sh) shows how: it mints a long-lived token secret and writes the kubeconfig. It's written for the compose stack, so use your cluster's API address, an admin kubeconfig and your own paths instead of its `k3s:6443`, `/k3s-admin/k3s.yaml`, `/k8s/` and `/out/`. Point `KUBECONFIG` at that file.
3. **Deliver the sandbox image.** With `DOCKER_HOST` and `KUBECONFIG` set, run `scripts/build-sandbox-image.sh` from the same tree as the server you build in step 5. Run it again after every upgrade of smelt. The image is named after the agent's sources (`smelt-sandbox:src-<hash>`), and a server with no `SANDBOX_IMAGE` runs the one named after the sources it was built from (SME-121), so it can't start pods from an older agent. If the image isn't on the node, `create_pod` fails within seconds with `ErrImageNeverPull`, naming the image and this command.
4. **Install headless Chrome:** `scripts/browser-check/setup.sh`. Then set `BROWSER_CHECK_CACHE` to the absolute path of the `.browser-check-cache` directory it creates.
5. **Build:** `dx bundle --platform web --release`. Keep the `--release`: without it dx builds a debug binary — its own output says `.../debug/web/` — which isn't what you want running for real. It puts a release server binary next to its web bundle, and dx's output says where. Run the binary from that layout: it serves the bundle from the `public/` directory beside it.
6. **Configure and start it.** Set the environment variables in [docs/setup/environment.md](setup/environment.md#environment-variables). The ones a deployment needs are:
   - `DATABASE_URL` and `KUBECONFIG`.
   - `PORT`: default `8080`.
   - `SMELT_BASE_URL`: the public address, used for MCP OAuth redirects and preview pages.
   - `SMELT_ALLOWED_HOSTS`: the host names smelt is reached by. Requests for any other host name are then refused. IP addresses and `localhost` always work, so this is no substitute for the firewall below.
   - `SMELT_PREVIEW_URL` and `SMELT_PREVIEW_ADDR`: where sandbox previews live.
   - `BROWSER_CHECK_CACHE`: from step 4.
7. **Put it behind TLS.** Use a reverse proxy that speaks HTTP/2: each tab holds an open event stream, and HTTP/1.1 allows only six connections per host. Route the preview host names (for example `{port}-{conversation}-smelt.example.com`) to `SMELT_PREVIEW_ADDR`'s port. They need wildcard DNS and a wildcard TLS certificate. The proxy must pass the browser's `Host` header through unchanged (in nginx, `proxy_set_header Host $host;`), since the preview listener reads the conversation and port from it, and must pass WebSocket upgrades through for a dev server's live reload. See [Sandbox previews](setup/sandbox.md#sandbox-previews).
8. **Add a model provider.** Open smelt, go to **Model providers** in the sidebar, and add one with its key. Nothing about the model is read from the environment.

### Deploying from published images

`.github/workflows/publish-images.yml` builds both images from one commit and pushes them to GHCR — every branch push publishes its own `<sha>` tags, and only main moves `latest`:

- `ghcr.io/<you>/smelt:<sha>` — the server: the release binary (26 MB, stripped by this repo's release profile), its web bundle, and the headless Chrome `webfetch` and browsing sessions use, already laid out the way `BROWSER_CHECK_CACHE` wants (set in the image, so a deployment doesn't set it). It listens on `8080` (and `8181` for previews), runs as uid 1000, and needs no volumes. About 885 MB, of which Chrome and the libraries it fetched are 492 MB.
- `ghcr.io/<you>/smelt-sandbox:<sha>` — the sandbox agent image, built from `docker/sandbox/Dockerfile` like any other image. It carries a `src-<hash>` tag as well: the same 16 hex digits `scripts/sandbox-image-ref` prints for the sources it was built from, under a registry a pod can actually reach.

One commit builds both, so a server and the agent it expects are the same build. These settings replace steps 1, 3, 4 and 5 above:

```bash
SANDBOX_IMAGE=ghcr.io/<you>/smelt-sandbox:<sha>          # not the src-<hash> default: that one
SANDBOX_IMAGE_PULL_POLICY=IfNotPresent                   # names nothing a pod can reach
SANDBOX_IMAGE_PULL_SECRET=ghcr                           # only when the package is private
# SANDBOX_DOCKER_IMAGE unset: docker:29-dind pulls from Docker Hub
```

`SANDBOX_IMAGE_PULL_POLICY` is the one that bites: pods ask `Never` unless told, so a sandbox from a registry image fails within seconds with `ErrImageNeverPull` — now an error that names the setting, not just the build script.

**Pull credentials.** A GHCR package is private until you make it public, and a private one needs the pod to present something, or both pulls fail with `pull access denied`. Make the secret in the namespace the pods are created in, and name it for them:

```bash
kubectl create secret docker-registry ghcr -n smelt-park \
  --docker-server=ghcr.io --docker-username=<you> --docker-password=<read-only-package-token>
```

A public GHCR package needs neither the secret nor the setting; the Docker sidecar's pull from Docker Hub is anonymous, so it needs nothing either. The secret is only ever read by sandbox pods, which is why it lives in `smelt-park` and not in whatever namespace smelt's own pod runs in.

`scripts/build-sandbox-image.sh`, `scripts/cluster-doctor` and the import loader are then dev and CI machinery: a deployment pulling images never runs them, and `cluster-doctor`'s `ctr images ls` check would report the node has no image it isn't looking for.

The builds themselves have been run: this Dockerfile produced the server image, which starts against a real Postgres, applies its migrations, and serves the app and its assets from the bundle beside it. And both images have been pulled back out of GHCR by tag and tried again as pulled — the server image against a database nothing had touched, where it applied all its migrations and served its own bundle's assets, and the sandbox image's agent, whose libraries all resolve. What hasn't been tried is the last step: a sandbox pod started from the published sandbox image against a real cluster.

### Before you expose it

smelt has **no login**. Anyone who can reach it can use your model keys and run commands in your sandboxes. And because the Docker sidecar is privileged, those commands can reach root on the sandbox's node. Keep smelt behind something that authenticates, such as a VPN or an authenticating proxy, and the preview host names with it. And make that the only way in: smelt listens on every interface (`0.0.0.0:$PORT`, and previews on `0.0.0.0:8181` by default). So firewall `PORT`, and either firewall the preview port or set `SMELT_PREVIEW_ADDR=127.0.0.1:8181`. [docs/setup/sandbox.md](setup/sandbox.md#docker-in-the-sandbox) has the details. Model provider keys, MCP servers' headers, OAuth tokens and client secrets, and the git SSH private key are stored in plain text in Postgres for now, so protect the database and its backups the same way.

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
- **Per-model settings**, on the provider's page: a context window (smelt compacts a conversation before it outgrows it, and the context indicator measures against it), **Max reply tokens** (the most one reply may be, thinking included; unset, what the provider reports, which only Anthropic's listing does, else 16,384 on an Anthropic or Other provider and no cap but the window on Ollama or llama.cpp once the window is known; on Anthropic it can't go above the listed cap; see [api.md](api.md) for how a turn's reply budget follows the room left, SME-111), on a llama.cpp provider a **Reasoning budget** (SME-111: a turn sends `thinking: {"type": "enabled", "budget_tokens": N}`, and llama.cpp forces the end of thinking at N tokens so the model answers; automatic is three quarters of the reply budget, leaving at least 4,096 tokens to answer, and a number here caps it; the other kinds keep adaptive thinking, since Anthropic refuses a budget on current models, Ollama doesn't enforce one, and Other may be a gateway in front of Claude) and thinking on or off. Unset, they come from what the provider reported, else a known `claude-*` model's window, else 200,000, shown as "unknown, assuming 200,000". Thinking defaults to on unless the provider says the model can't. `run_turn_bounded` still retries without thinking if a request fails with Ollama's "error parsing tool call" (see [api.md](api.md)).

## Environment Variables

In [setup/environment.md](setup/environment.md).

## Tracing (OpenTelemetry)

In [setup/environment.md](setup/environment.md).

## Built-in MCP servers

In [setup/sandbox.md](setup/sandbox.md).

## Sandbox previews

In [setup/sandbox.md](setup/sandbox.md).

## Docker in the sandbox

In [setup/sandbox.md](setup/sandbox.md).

## Git in the sandbox

In [setup/sandbox.md](setup/sandbox.md).

## Language servers

In [setup/sandbox.md](setup/sandbox.md).

## Pod metrics

In [setup/sandbox.md](setup/sandbox.md).

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
