FROM rust:1.96-trixie AS base

RUN apt-get update && apt-get install -y \
    curl \
    xz-utils \
    git \
    binaryen \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

# GitHub CLI, via its official apt repo — needed to open PRs from inside
# the container (see the retrospectives on Done tickets in Linear: this was previously
# a manual per-session install).
RUN mkdir -p -m 755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && mkdir -p -m 755 /etc/apt/sources.list.d \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y gh \
    && rm -rf /var/lib/apt/lists/*

# Docker CLI only (no daemon) — talks to the `docker` sidecar service in
# docker-compose.yml over DOCKER_HOST, not a locally running daemon.
# Pinned to match that sidecar's major version (docker:29-dind).
RUN curl -fsSL https://download.docker.com/linux/static/stable/x86_64/docker-29.6.2.tgz \
    | tar -xz --strip-components=1 -C /usr/local/bin docker/docker

# Playwright (Python), for driving/screenshotting the running app in a real
# (headless) browser — e.g. to verify a UI change actually renders, not just
# that it compiles. Kept out of the project's own Cargo/Node toolchain since
# it's a dev-container capability, not an app dependency (the app has no
# Node.js dependency at all). `install-deps` pulls in Chromium's system
# shared libraries and needs root; the browser binary itself is fetched
# later as `dev` into that user's own cache dir.
RUN python3 -m venv /opt/playwright-venv \
    && /opt/playwright-venv/bin/pip install --no-cache-dir playwright \
    && /opt/playwright-venv/bin/playwright install-deps chromium \
    && rm -rf /var/lib/apt/lists/*

ARG UID=1000
ARG GID=1000

RUN groupadd -g ${GID} dev && \
    useradd -m -u ${UID} -g ${GID} -s /bin/bash dev

RUN chown -R dev:dev /opt/playwright-venv

USER dev
ENV USER=dev

# Add WASM target for the Dioxus web/client build
RUN rustup target add wasm32-unknown-unknown
RUN rustup component add rustfmt
# scripts/lint-expects (SME-95)
RUN rustup component add clippy

# Downloads into ~/.cache/ms-playwright — dev-owned, no root needed for this
# part. `/opt/playwright-venv/bin/playwright`/`python` is the entry point for
# scripting it (e.g. `playwright install chromium` already ran the deps half
# above; a page-screenshot script just imports `playwright.sync_api`).
RUN /opt/playwright-venv/bin/playwright install chromium

# Persist bash history to a mountable directory
RUN mkdir -p /home/dev/.bash_history_dir && \
    echo 'export HISTFILE=/home/dev/.bash_history_dir/.bash_history' >> /home/dev/.bashrc

# Pre-create the gh config dir owned by `dev` so the gh-config volume (see
# docker-compose.yml) inherits correct ownership on first mount. Without
# this, Docker auto-creates the mount point as root (nothing in the image
# writes here otherwise — gh is installed as a system package before `USER
# dev` is even set) and `gh auth login` can complete the OAuth flow but
# fails to persist the token, silently leaving the container logged out.
RUN mkdir -p /home/dev/.config/gh

# Install cargo-binstall for fast prebuilt binary installs
RUN curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

# Claude Code CLI (native installer — no Node.js dependency, installs to
# ~/.local/bin which is already on PATH by default for this user).
RUN curl -fsSL https://claude.ai/install.sh | bash

# Pinned to match the `dioxus` crate version in Cargo.toml — a mismatched
# `dx` CLI refuses to serve/build the project at all.
RUN cargo binstall -y --locked dioxus-cli@0.7.9

WORKDIR /app

# Dev target: adds sqlx-cli for local migration/query work (slow to compile,
# not needed just to build or run the app).
FROM base AS dev
RUN cargo binstall -y sqlx-cli

# ── Deployment images ───────────────────────────────────────────────────────

# Everything above is the build and dev environment; these stages make what
# you actually run. The sandbox image is built the same way but from its own
# Dockerfile (docker/sandbox/Dockerfile) — it has no use for any of this
# toolchain beyond the agent binary. .github/workflows/publish-images.yml
# builds both from one commit and pushes them; docs/setup.md's "Deploying"
# says what to point a deployment at.
#
# Build the server image from the repo root:
#   docker build -f Dockerfile --target runtime -t smelt:dev .
# The release build is the long part: dx compiles the app once for wasm and
# once for the server, through this repo's fat-LTO release profile. dx's
# server half is also known to hang after it has written the bundle (SME-67),
# so a build that sits quiet long after "Client build completed successfully"
# has hung there — which is what the publish workflow's job timeout is for.

# The build environment the published images need, and nothing else — a
# toolchain with the wasm target and the pinned dx. Deliberately not
# `FROM base`: base is the dev container, and publishing an image shouldn't
# depend on Playwright's Chromium CDN being reachable. Same `rust:1.96-trixie`
# as base, so the same glibc — the one thing a binary built here has to match
# to run in the images below (docs/setup.md).
FROM rust:1.96-trixie AS build-env
RUN apt-get update \
    && apt-get install -y --no-install-recommends git curl ca-certificates binaryen \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
# cargo-binstall, then dx at the version the `dioxus` crate pins — a mismatched
# `dx` refuses to build the project at all. binstall installs into
# $CARGO_HOME/bin, which is on every user's PATH in the rust image.
RUN curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh \
        | bash \
    && cargo binstall -y --locked dioxus-cli@0.7.9

WORKDIR /app

# The release server binary and its web bundle.
FROM build-env AS server-build
COPY . .
# dx prints where it put things, but the path has moved between dx versions
# and isn't something to bake into this file. So: find the web bundle, and
# take the executable beside it. Loudly give up with what's actually there if
# that doesn't work, rather than building an image that can only fail at
# startup.
RUN dx bundle --platform web --release \
    && { public="$(find target/dx dist -type d -name public 2>/dev/null | head -n1)"; \
         test -n "$public" || { echo "error: no web bundle under target/dx or dist — see dx's output above"; ls -R target/dx 2>/dev/null | head -40; exit 1; }; \
         bindir="$(dirname "$public")"; \
         server="$(find "$bindir" -maxdepth 1 -type f -perm -u+x 2>/dev/null | head -n1)"; \
         test -n "$server" || { echo "error: no executable beside $public"; ls -l "$bindir"; exit 1; }; \
         install -d /app/image/smelt; \
         cp -a "$bindir/." /app/image/smelt/; \
         mv "/app/image/smelt/$(basename "$server")" /app/image/smelt/smelt; \
         echo "server image: $server (renamed smelt) with the bundle from $public"; }

# Headless Chrome for webfetch and browsing sessions, laid out the way smelt
# reads it from BROWSER_CHECK_CACHE. Deliberately fetched on
# debian:trixie-slim and not on `base`: setup.sh decides which shared
# libraries it needs from what `ldd` reports missing where it runs, so running
# it on a fat build image would record a set of libraries missing from
# nothing. Checked on trixie-slim: with the fetched libdir on
# LD_LIBRARY_PATH every library resolves and chrome-headless-shell renders a
# page. Same glibc as the server image, which is what the sandbox agent's
# glibc has to match anyway (docs/setup.md).
FROM debian:trixie-slim AS chrome
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl python3 unzip \
    && rm -rf /var/lib/apt/lists/*
COPY scripts/browser-check/setup.sh /build/smelt/scripts/browser-check/setup.sh
# The absolute path the runtime stage sets below and a deployment sets in
# BROWSER_CHECK_CACHE; setup.sh downloads into it rather than into its
# (imaginary) checkout.
ENV BROWSER_CHECK_CACHE=/opt/browser-check-cache
# setup.sh keeps the .deb files and apt's scratch dirs it fetched them with
# under the cache directory — nothing needs them once the libraries are
# unpacked, and they're most of what this stage would otherwise add.
RUN bash /build/smelt/scripts/browser-check/setup.sh \
    && rm -rf "$BROWSER_CHECK_CACHE/debs" "$BROWSER_CHECK_CACHE/apt"

# The server image. `ca-certificates` because every model provider, MCP server
# and price-catalog fetch is HTTPS; `fonts-liberation` (with fontconfig, as a
# dependency) because a browsing session shows the user the page it rendered,
# and a slim base with no fonts renders text as blanks.
FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates fonts-liberation \
    && rm -rf /var/lib/apt/lists/*
# smelt writes nothing but Chrome's per-launch profile under /tmp; uid pinned
# to 1000 so a mounted volume's ownership is predictable, same as everywhere
# else in this repo's images.
RUN groupadd -g 1000 smelt && useradd -m -u 1000 -g 1000 -s /bin/bash smelt
COPY --from=server-build --chown=smelt:smelt /app/image/smelt/ /opt/smelt/
COPY --from=chrome --chown=smelt:smelt /opt/browser-check-cache /opt/browser-check-cache
# Where the server finds its web bundle. The default is a `public` directory
# beside the executable; named here so it doesn't depend on how the binary
# was launched.
ENV DIOXUS_PUBLIC_PATH=/opt/smelt/public \
    BROWSER_CHECK_CACHE=/opt/browser-check-cache \
    PORT=8080
USER smelt
WORKDIR /opt/smelt
# 8080 is the app; 8181 is the sandbox preview listener, a separate one of its
# own (docs/setup.md, "Sandbox previews").
EXPOSE 8080 8181
RUN test -x /opt/smelt/smelt && test -d /opt/smelt/public && test -x /opt/browser-check-cache/chrome/chrome-headless-shell-linux64/chrome-headless-shell
ENTRYPOINT ["/opt/smelt/smelt"]
