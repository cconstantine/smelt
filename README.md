# smelt

A self-hosted, single-user AI coding agent, written entirely in Rust.

Every conversation is a coding session. The model gets a sandbox of its own, a Kubernetes pod with terminals, Docker and git. There it writes, runs and fixes code, and you watch and steer it from a chat in your browser. smelt talks to Claude through the Anthropic Messages API, or to any server that speaks it (Ollama, llama.cpp, a gateway).

![A conversation: the model's tool calls, a file edit, a markdown reply and its todo list](docs/images/chat.png)

## Features

- **Live chat.** Replies stream in token by token and render as markdown, with highlighted code, tables and task lists. Each tool call is one line saying what it did, with its result a click away, and a file edit shows as a diff. Every open tab sees the same conversation live. You can stop a turn at any time. Each conversation has its own URL, and history is kept in Postgres.
- **A sandbox per conversation.** The model starts a pod with as many persistent terminals as it needs and runs real commands in them. A live panel shows each terminal's output as it's written, and the model is told when a command finishes. It reads, edits and searches files (`read_file`, `edit_file`, `glob`, `grep`) and runs Docker (`docker build`, `docker compose`) in a sidecar. It also clones git repos with an SSH key you set up in the app. A conversation's `/workspace` outlives its pods.
- **Previews.** A dev server running in the sandbox opens in your browser through a preview link, and in the model's own browser at `localhost`.
- **The web.** `webfetch` reads a page through a real headless browser, and `http_request` calls an API directly. Both go through a guard against private addresses. Browsing sessions let the model click and type through a site while a live panel shows you the same page, and you can use it too. Web search comes built in, through Exa's hosted MCP server.
- **Planning and questions.** The model keeps a todo list you can see, and it can stop to ask you a multiple-choice or free-text question.
- **Context.** A meter shows how full the model's context window is, with a breakdown of what's in it. A long conversation is compacted automatically before it overflows.
- **Model providers.** You add providers in the app: Anthropic, Ollama, llama.cpp or any Anthropic-compatible server. Each conversation picks its own model, and each call's token usage and cost are recorded.
- **MCP servers.** Any hosted MCP server's tools are available in every conversation. A server can sign in with a static header or with OAuth.
- **Language servers.** You configure language servers as data, filled in from mason's registry, and the model starts them next to its sandbox for diagnostics and code navigation.
- **Housekeeping.** The Sandboxes page lists every live pod with its memory and CPU use, and stops any of them. A tab left open across a deploy says it's out of date. There's a dark mode, and the layout works at phone width.

The full, current feature list is the project's "Current state" document in Linear. [docs/architecture.md](docs/architecture.md) describes how it's built.

## Screenshots

These were taken from smelt's automated browser test setup, with made-up conversations and a stand-in model.

The sandbox panel, with a live terminal in the conversation's pod:

![A conversation with its sandbox panel open, showing a terminal's commands and output](docs/images/sandbox.png)

The Sandboxes page:

![The Sandboxes page, listing a live pod with its conversation, status, uptime and resource use](docs/images/sandboxes.png)

Dark mode follows the system setting:

![The same conversation in dark mode](docs/images/chat-dark.png)

## Deploying

smelt is one server binary plus a web bundle. Alongside it, it needs:

- **Postgres.** smelt applies its own migrations at startup.
- **A single-node k3s cluster** for the sandboxes, with a default StorageClass (k3s ships `local-path`). It has to be k3s on one node: the sandbox image is imported straight into that node's containerd at k3s's socket path, with no registry, and pods never pull it. smelt runs the sandboxes in the `smelt-park` namespace under a `park` service account, and [k8s/smelt-park-rbac.yaml](k8s/smelt-park-rbac.yaml) creates both. The cluster must allow privileged pods, because each sandbox's Docker sidecar is privileged. Live memory and CPU on the Sandboxes page also need metrics-server.
- **The sandbox image**, delivered straight to the node's containerd with no registry involved.
- **Headless Chrome**, for `webfetch` and browsing sessions.

### Steps

1. **Install the toolchain:** Rust, the `wasm32-unknown-unknown` target and the Dioxus CLI (`dx`). The repo's [Dockerfile](Dockerfile) sets up the same toolchain for the dev container.
2. **Set up the cluster.** Run `kubectl apply -f k8s/smelt-park-rbac.yaml`, then make a kubeconfig for the `park` service account. [scripts/k3s-bootstrap.sh](scripts/k3s-bootstrap.sh) shows how: it mints a long-lived token secret and writes the kubeconfig. Point `KUBECONFIG` at that file.
3. **Deliver the sandbox image.** With `DOCKER_HOST` and `KUBECONFIG` set, run `scripts/build-sandbox-image.sh --latest`. Run it again after any change to the sandbox agent (`src/bin/sandbox_agent.rs`, `docker/sandbox/`).
4. **Install headless Chrome:** `scripts/browser-check/setup.sh`. Then set `BROWSER_CHECK_CACHE` to the absolute path of the `.browser-check-cache` directory it creates.
5. **Build:** `dx bundle --platform web`. It produces a release server binary next to its web bundle, and dx's output says where. Run the binary from that layout: it serves the bundle from the `public/` directory beside it.
6. **Configure and start it.** Set the environment variables in [docs/setup.md](docs/setup.md#environment-variables). The ones a deployment needs are:
   - `DATABASE_URL` and `KUBECONFIG`.
   - `PORT`: default `8080`.
   - `SMELT_BASE_URL`: the public address, used for MCP OAuth redirects and preview pages.
   - `SMELT_ALLOWED_HOSTS`: the host names smelt is reached by. Requests for any other host are then refused.
   - `SMELT_PREVIEW_URL` and `SMELT_PREVIEW_ADDR`: where sandbox previews live.
   - `BROWSER_CHECK_CACHE`: from step 4.
7. **Put it behind TLS.** Use a reverse proxy that speaks HTTP/2: each tab holds an open event stream, and HTTP/1.1 allows only six connections per host. Route the preview host names (for example `{port}-{conversation}-smelt.example.com`) to `SMELT_PREVIEW_ADDR`'s port. See [Sandbox previews](docs/setup.md#sandbox-previews).
8. **Add a model provider.** Open smelt, go to **Model providers** in the sidebar, and add one with its key. Nothing about the model is read from the environment.

### Before you expose it

smelt has **no login**. Anyone who can reach it can use your model keys and run commands in your sandboxes. And because the Docker sidecar is privileged, those commands can reach root on the sandbox's node. Keep smelt behind something that authenticates, such as a VPN or an authenticating proxy, and the preview host names with it. [docs/setup.md](docs/setup.md#docker-in-the-sandbox) has the details.

### Trying it locally

[docker-compose.yml](docker-compose.yml) brings up a dev stack: Postgres, a k3s cluster with the RBAC already applied, a Docker daemon, and a container to build and run smelt in.

```bash
docker compose up -d
docker compose exec smelt bash
# inside the container:
scripts/build-sandbox-image.sh --latest
scripts/browser-check/setup.sh
dx serve --fullstack --addr 0.0.0.0 --port 8080
```

Then open <http://localhost:8180>. Previews are on port 8181. [Dev over HTTPS](docs/setup.md#dev-over-https-http2) covers the HTTPS address (`https://localhost:8443`).

## Documentation

| Topic | File |
|---|---|
| Build, run, environment variables | [docs/setup.md](docs/setup.md) |
| Module map, request flow, feature flags | [docs/architecture.md](docs/architecture.md) |
| Database and schema | [docs/database.md](docs/database.md) |
| Server functions and streaming | [docs/api.md](docs/api.md) |
| Data models | [docs/models.md](docs/models.md) |
| MCP servers | [docs/mcp.md](docs/mcp.md) |
| The web UI | [docs/frontend.md](docs/frontend.md) |
| Tests | [docs/testing.md](docs/testing.md) |
| How work is planned, built and reviewed | [docs/development-process.md](docs/development-process.md) |

Work is tracked as tickets in Linear (`SME-N` ids in commits and comments).
