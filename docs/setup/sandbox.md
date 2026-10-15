# Sandbox setup

Part of [setup.md](../setup.md). MCP servers, previews, Docker, git, language servers and pod metrics in the sandbox.

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
- **Containers get the pod's uplink MTU** (SME-157). Docker's own default MTU is 1500; on Flannel's VXLAN (k3s's default) the pod's uplink is 1450, and a container advertising MSS 1460 loses every full-MSS segment its peer sends — `git clone` over HTTPS dies on a recv failure, `docker build` fails in whichever `RUN` step reaches out, and it looks like a problem with the site. `start-dockerd.sh` reads the MTU of the interface carrying the pod's default route from `/proc/net/route` and sysfs and starts dockerd with both `--mtu` (the default bridge) and `--default-network-opt bridge=com.docker.network.driver.mtu=` (user-defined bridges — what compose project networks are); neither flag alone covers both. If no uplink MTU can be read the sidecar refuses to start dockerd, with the reason in its container log, rather than come up broken for some hosts. Networks already stored in `/var/lib/docker` keep the MTU they were created with, and nothing migrates them: a user-created network gets the new one when it's deleted and recreated (a compose project's, with `docker compose down` then `up`), and the default bridge on a conversation's Docker volume created before this fix keeps its stored MTU until that volume's Docker data is reset.

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
