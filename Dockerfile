FROM rust:1.96-trixie AS base

RUN apt-get update && apt-get install -y \
    curl \
    xz-utils \
    git \
    binaryen \
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

ARG UID=1000
ARG GID=1000

RUN groupadd -g ${GID} dev && \
    useradd -m -u ${UID} -g ${GID} -s /bin/bash dev

USER dev
ENV USER=dev

# Add WASM target for the Dioxus web/client build
RUN rustup target add wasm32-unknown-unknown
RUN rustup component add rustfmt

# Persist bash history to a mountable directory
RUN mkdir -p /home/dev/.bash_history_dir && \
    echo 'export HISTFILE=/home/dev/.bash_history_dir/.bash_history' >> /home/dev/.bashrc

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
