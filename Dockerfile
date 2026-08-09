FROM rust:1.96-trixie AS base

RUN apt-get update && apt-get install -y \
    curl \
    xz-utils \
    git \
    binaryen \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

# GitHub CLI, via its official apt repo — needed to open PRs from inside
# the container (see docs/projects/completed/ retros: this was previously
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
