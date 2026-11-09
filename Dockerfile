# dx's tools, so that dx fetches none of them at bundle time (SME-413). dx
# 0.7.9 otherwise downloads wasm-bindgen and esbuild on every bundle, and
# wasm-opt on every release one, from GitHub and npm, and takes an error
# page for an archive: it logs "wasm-opt installed from Github" and then
# fails running a file it never wrote. Here each comes from crates.io or
# npm, pinned and checked, and fails loudly; `base` and `bundle-env` copy
# them in and set NO_DOWNLOADS. wasm-opt is Debian's binaryen, which `base`
# and `build-env` already apt-install. Same image as `base` and `build-env`, so the
# same glibc.
FROM rust:1.96-trixie AS dx-tools
# Must equal Cargo.lock's `wasm-bindgen` (dx refuses any other version);
# src/repo_lints.rs checks it.
ARG WASM_BINDGEN_VERSION=0.2.126
# dioxus-cli 0.7.9's ESBUILD_VERSION: bump it with `dioxus-cli@0.7.9` below
# (dx doesn't check esbuild's version), and re-check wasm-opt then too.
ARG ESBUILD_VERSION=0.27.3
RUN cargo install --locked --root /opt/dx-tools wasm-bindgen-cli@${WASM_BINDGEN_VERSION}
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) plat=linux-x64; sha=066e20cdb882994160e18524a552b97e03648eb9aa0c7cdf5680a6493be65ab2 ;; \
        aarch64) plat=linux-arm64; sha=04f0bfb132b8b0800c23b22caa9ad7a7adf41e2434c027fc8571318b9904712f ;; \
        *) echo "error: no esbuild pin for $(uname -m)"; exit 1 ;; \
    esac; \
    work="$(mktemp -d)"; cd "$work"; \
    curl -fsSL --retry 5 -o esbuild.tgz "https://registry.npmjs.org/@esbuild/$plat/-/$plat-${ESBUILD_VERSION}.tgz"; \
    echo "$sha  esbuild.tgz" | sha256sum -c -; \
    tar -xzf esbuild.tgz package/bin/esbuild; \
    install -m 0755 package/bin/esbuild /opt/dx-tools/bin/esbuild; \
    cd /; rm -rf "$work"
RUN /opt/dx-tools/bin/wasm-bindgen --version && /opt/dx-tools/bin/esbuild --version

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
RUN curl -fsSL "https://download.docker.com/linux/static/stable/$(uname -m)/docker-29.6.2.tgz" \
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

# dx's tools from the dx-tools stage, and dx told to use them from PATH and
# never download (dioxus-cli's `prefer_no_downloads`, SME-413). Debian's
# wasm-opt is the one dx now runs: a binaryen bump is a deliberate change.
COPY --from=dx-tools /opt/dx-tools/bin/wasm-bindgen /opt/dx-tools/bin/esbuild /usr/local/bin/
ENV NO_DOWNLOADS=1
RUN wasm-opt --version | grep -q '^wasm-opt version 120'

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

# build-env plus dx's bundle tools, the same as base's: from dx-tools, with no
# downloads (SME-413). A stage of its own so that publish-images' sandbox-image
# job, which builds build-env and never runs dx, doesn't build dx-tools.
FROM build-env AS bundle-env
COPY --from=dx-tools /opt/dx-tools/bin/wasm-bindgen /opt/dx-tools/bin/esbuild /usr/local/bin/
ENV NO_DOWNLOADS=1
RUN wasm-opt --version | grep -q '^wasm-opt version 120'

# The release server binary and its web bundle: docker/server-bundle.sh,
# which scripts/test-dx-offline also runs, with GitHub and npm blocked.
FROM bundle-env AS server-build
COPY . .
RUN docker/server-bundle.sh

# Headless Chromium (Debian's chromium-headless-shell, SME-360) for webfetch
# and browsing sessions, laid out the way smelt reads it from
# BROWSER_CHECK_CACHE. Deliberately fetched on debian:trixie-slim and not on
# `base`: setup.sh fetches apt's dependency closure of the package, and
# building it where nothing extra is installed keeps that honest. It was not
# always honest: python3, which setup.sh itself needs,
# installs libexpat1 and libffi8, and a check run in this stage passes on
# them while the runtime stage below — which has no python3 — does not have
# them at all. That's why the real check is a RUN of Chrome in the runtime
# stage, and why PKGS in setup.sh names both. Same glibc as the server
# image, which is what the sandbox agent's glibc has to match anyway
# (docs/setup.md).
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

# Claude Code, for the Claude Code provider kind (SME-429): the official
# native binary at a pinned version, at /opt/claude/bin/claude. Fetched from
# Anthropic's release bucket and verified the way Claude Code's setup docs
# ("Binary integrity and code signing") describe: the release manifest's
# detached signature against Anthropic's release-signing key, pinned here by
# its fingerprint, then the binary against the manifest's SHA-256. The
# signature is checked by gpgv against a keyring of the pinned key alone:
# `gpg --verify` takes a good signature from any key the .asc carried, so a
# second key in it could sign a manifest of its own (review 1 M1). Not the
# docs' `install.sh | bash -s <version>` route: that runs the latest release
# to install the pinned one under a home directory, behind a launcher meant
# to update itself. Bump CLAUDE_CODE_VERSION to upgrade; the runtime stage
# turns the autoupdater off and runs `claude --version`.
FROM debian:trixie-slim AS claude
ARG CLAUDE_CODE_VERSION=2.1.318
ARG CLAUDE_CODE_KEY_FINGERPRINT=31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gnupg gpgv jq \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) platform=linux-x64 ;; \
        aarch64) platform=linux-arm64 ;; \
        *) echo "error: no Claude Code build for $(uname -m)"; exit 1 ;; \
    esac; \
    repo=https://downloads.claude.ai/claude-code-releases; \
    work="$(mktemp -d)"; cd "$work"; \
    export GNUPGHOME="$work/gnupg"; mkdir -m 700 "$GNUPGHOME"; \
    curl -fsSL https://downloads.claude.ai/keys/claude-code.asc | gpg --batch --import; \
    gpg --batch --with-colons --fingerprint | grep -qx "fpr:::::::::${CLAUDE_CODE_KEY_FINGERPRINT}:" \
        || { echo "error: Claude Code's release key isn't $CLAUDE_CODE_KEY_FINGERPRINT"; exit 1; }; \
    gpg --batch --export "$CLAUDE_CODE_KEY_FINGERPRINT" > pinned.gpg; \
    curl -fsSLO "$repo/$CLAUDE_CODE_VERSION/manifest.json"; \
    curl -fsSLO "$repo/$CLAUDE_CODE_VERSION/manifest.json.sig"; \
    gpgv --keyring "$work/pinned.gpg" manifest.json.sig manifest.json \
        || { echo "error: the manifest isn't signed by $CLAUDE_CODE_KEY_FINGERPRINT"; exit 1; }; \
    test "$(jq -r .version manifest.json)" = "$CLAUDE_CODE_VERSION"; \
    checksum="$(jq -r --arg p "$platform" '.platforms[$p].checksum' manifest.json)"; \
    curl -fsSL -o claude "$repo/$CLAUDE_CODE_VERSION/$platform/claude"; \
    echo "$checksum  claude" | sha256sum -c -; \
    install -D -m 0755 claude /opt/claude/bin/claude; \
    cd /; rm -rf "$work"

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
COPY --from=claude /opt/claude /opt/claude
# Where the server finds its web bundle. The default is a `public` directory
# beside the executable; named here so it doesn't depend on how the binary
# was launched.
# Claude Code on PATH, as a Claude Code provider's default `claude` finds
# it, and never updating itself: the image pins its version (SME-429).
ENV DIOXUS_PUBLIC_PATH=/opt/smelt/public \
    BROWSER_CHECK_CACHE=/opt/browser-check-cache \
    PORT=8080 \
    PATH=/opt/claude/bin:$PATH \
    DISABLE_AUTOUPDATER=1
USER smelt
WORKDIR /opt/smelt
# 8080 is the app; 8181 is the sandbox preview listener, a separate one of its
# own (docs/setup.md, "Sandbox previews").
EXPOSE 8080 8181
# Not just that Chrome is present and executable: actually run it, the way
# src/headless_chrome.rs launches it. `test -x` here let through images whose
# Chrome died at the first webfetch/browsing call — a cache laid down by an
# incomplete library list satisfies the build stage it was made in (see the
# chrome stage's comment) and nothing like it exists to check against at
# runtime. `--version` costs nothing and forces the dynamic linker to
# resolve every library Chrome needs; if the image is missing one, the
# image build fails here instead of the user's first browse.
RUN test -x /opt/smelt/smelt && test -d /opt/smelt/public \
    && lib="/opt/browser-check-cache/libs/usr/lib/$(uname -m)-linux-gnu" \
    && LD_LIBRARY_PATH="$lib:$lib/dri" /opt/browser-check-cache/libs/usr/lib/chromium/chromium-headless-shell --version
# The same for Claude Code: it runs here, as the server's user, or the build
# fails rather than a provider's status line.
RUN claude --version
ENTRYPOINT ["/opt/smelt/smelt"]
