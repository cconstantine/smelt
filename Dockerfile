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
# Every download in these stages goes through scripts/fetch-pinned: to a
# temp file with retries and bounds, checked against its sha256, then
# renamed, never piped (SME-470; src/repo_lints/fetches.rs checks it).
COPY scripts/fetch-pinned /usr/local/bin/fetch-pinned
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) plat=linux-x64; sha=066e20cdb882994160e18524a552b97e03648eb9aa0c7cdf5680a6493be65ab2 ;; \
        aarch64) plat=linux-arm64; sha=04f0bfb132b8b0800c23b22caa9ad7a7adf41e2434c027fc8571318b9904712f ;; \
        *) echo "error: no esbuild pin for $(uname -m)"; exit 1 ;; \
    esac; \
    work="$(mktemp -d)"; cd "$work"; \
    fetch-pinned "https://registry.npmjs.org/@esbuild/$plat/-/$plat-${ESBUILD_VERSION}.tgz" esbuild.tgz "$sha"; \
    tar -xzf esbuild.tgz package/bin/esbuild; \
    install -m 0755 package/bin/esbuild /opt/dx-tools/bin/esbuild; \
    cd /; rm -rf "$work"
RUN /opt/dx-tools/bin/wasm-bindgen --version && /opt/dx-tools/bin/esbuild --version

# dx itself, at the version the `dioxus` crate pins (a mismatched dx refuses
# to build the project; src/repo_lints/fetches.rs checks DX_VERSION against
# Cargo.lock). DioxusLabs' release tarball, checked against the sha256 in
# its published dx-<arch>-unknown-linux-gnu.sha256 (SME-470). This replaces
# cargo-binstall, whose installer came from its `main` branch piped into
# bash and itself installed binstall's latest release. Bump DX_VERSION with
# both sha256s, and re-check ESBUILD_VERSION above.
FROM rust:1.96-trixie AS dx-cli
ARG DX_VERSION=0.7.9
COPY scripts/fetch-pinned /usr/local/bin/fetch-pinned
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) sha=3b132551b480bc96f938f9f0d37936ee1190f994977539dcc347eaf38540d005 ;; \
        aarch64) sha=8cf14db0b11b43b31dd6d39e71b00e567f2fccfde85ae3a8f7ef0f8745e5ccfb ;; \
        *) echo "error: no dx pin for $(uname -m)"; exit 1 ;; \
    esac; \
    work="$(mktemp -d)"; cd "$work"; \
    fetch-pinned "https://github.com/DioxusLabs/dioxus/releases/download/v${DX_VERSION}/dx-$(uname -m)-unknown-linux-gnu.tar.gz" dx.tar.gz "$sha"; \
    tar -xzf dx.tar.gz dx; \
    install -D -m 0755 dx /opt/dx-cli/bin/dx; \
    cd /; rm -rf "$work"
RUN v="$(/opt/dx-cli/bin/dx --version)" && echo "$v" && case "$v" in "dioxus ${DX_VERSION} "*) ;; \
        *) echo "error: the dx tarball isn't dioxus ${DX_VERSION}: $v"; exit 1 ;; esac

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
# Each download through scripts/fetch-pinned, for its retries and bounds
# (SME-470): the key, manifest and signature with `-`, each checked on the
# next line as before (the key by its fingerprint, the manifest by gpgv),
# and the binary against the signed manifest's SHA-256.
COPY scripts/fetch-pinned /usr/local/bin/fetch-pinned
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) platform=linux-x64 ;; \
        aarch64) platform=linux-arm64 ;; \
        *) echo "error: no Claude Code build for $(uname -m)"; exit 1 ;; \
    esac; \
    repo=https://downloads.claude.ai/claude-code-releases; \
    work="$(mktemp -d)"; cd "$work"; \
    export GNUPGHOME="$work/gnupg"; mkdir -m 700 "$GNUPGHOME"; \
    fetch-pinned https://downloads.claude.ai/keys/claude-code.asc claude-code.asc -; \
    gpg --batch --import claude-code.asc; \
    gpg --batch --with-colons --fingerprint | grep -qx "fpr:::::::::${CLAUDE_CODE_KEY_FINGERPRINT}:" \
        || { echo "error: Claude Code's release key isn't $CLAUDE_CODE_KEY_FINGERPRINT"; exit 1; }; \
    gpg --batch --export "$CLAUDE_CODE_KEY_FINGERPRINT" > pinned.gpg; \
    fetch-pinned "$repo/$CLAUDE_CODE_VERSION/manifest.json" manifest.json -; \
    fetch-pinned "$repo/$CLAUDE_CODE_VERSION/manifest.json.sig" manifest.json.sig -; \
    gpgv --keyring "$work/pinned.gpg" manifest.json.sig manifest.json \
        || { echo "error: the manifest isn't signed by $CLAUDE_CODE_KEY_FINGERPRINT"; exit 1; }; \
    test "$(jq -r .version manifest.json)" = "$CLAUDE_CODE_VERSION"; \
    checksum="$(jq -r --arg p "$platform" '.platforms[$p].checksum' manifest.json)"; \
    fetch-pinned "$repo/$CLAUDE_CODE_VERSION/$platform/claude" claude "$checksum"; \
    install -D -m 0755 claude /opt/claude/bin/claude; \
    cd /; rm -rf "$work"

FROM rust:1.96-trixie AS base

RUN apt-get update && apt-get install -y \
    curl \
    xz-utils \
    git \
    binaryen \
    python3-venv \
    && rm -rf /var/lib/apt/lists/*

COPY scripts/fetch-pinned /usr/local/bin/fetch-pinned

# GitHub CLI, via its official apt repo — needed to open PRs from inside
# the container (see the retrospectives on Done tickets in Linear: this was previously
# a manual per-session install). The keyring is pinned by the SHA256 that
# cli/cli's docs/install_linux.md publishes for it (holding keys
# 2C6106201985B60E6C7AC87323F3D4EA75716059 and
# 7F38BBB59D064DBCB3D84D725612B36462313325), not trusted on first use
# (SME-470); a new keyring there means a new pin here.
RUN mkdir -p -m 755 /etc/apt/keyrings \
    && fetch-pinned https://cli.github.com/packages/githubcli-archive-keyring.gpg /etc/apt/keyrings/githubcli-archive-keyring.gpg \
        6084d5d7bd8e288441e0e94fc6275570895da18e6751f70f057485dc2d1a811b \
    && mkdir -p -m 755 /etc/apt/sources.list.d \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y gh \
    && rm -rf /var/lib/apt/lists/*

# Docker CLI only (no daemon) — talks to the `docker` sidecar service in
# docker-compose.yml over DOCKER_HOST, not a locally running daemon.
# Pinned to match that sidecar's major version (docker:29-dind). Docker
# publishes no checksum for its static builds, so each sha256 here was
# taken from a download on 2026-11-09 (SME-470); bump them with the version.
ARG DOCKER_CLI_VERSION=29.6.2
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) sha=d6204aea92238e2453d5445c885b9d2e5eb8f82915568ec50edf9dbe12a3ac74 ;; \
        aarch64) sha=8d16d8b3b158c132a9fb9963d4b4345746f925e287e154c9ed880ac257baf292 ;; \
        *) echo "error: no docker CLI pin for $(uname -m)"; exit 1 ;; \
    esac; \
    work="$(mktemp -d)"; \
    fetch-pinned "https://download.docker.com/linux/static/stable/$(uname -m)/docker-${DOCKER_CLI_VERSION}.tgz" "$work/docker.tgz" "$sha"; \
    tar -xzf "$work/docker.tgz" --strip-components=1 -C /usr/local/bin docker/docker; \
    rm -rf "$work"; \
    docker --version

# Playwright (Python), for driving/screenshotting the running app in a real
# (headless) browser — e.g. to verify a UI change actually renders, not just
# that it compiles. Kept out of the project's own Cargo/Node toolchain since
# it's a dev-container capability, not an app dependency (the app has no
# Node.js dependency at all). `install-deps` pulls in Chromium's system
# shared libraries and needs root; the browser binary itself is fetched
# later as `dev` into that user's own cache dir. Pinned (SME-470), which
# also pins the Chromium build its downloader fetches: 1.64.1 is what an
# unpinned install resolved on 2026-11-09. No hash pin: pip's
# --require-hashes needs every dependency's hash, and only the dev image
# has Playwright. pip retries itself (5 times, 15 s timeout), and so does
# `playwright install` (5 attempts across its mirrors:
# microsoft/playwright v1.64.1, browserFetcher.ts:50).
RUN python3 -m venv /opt/playwright-venv \
    && /opt/playwright-venv/bin/pip install --no-cache-dir playwright==1.64.1 \
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

# Claude Code CLI: the claude stage's binary, at its pinned version and
# verified against Anthropic's signed manifest, the same one the server
# image ships (SME-470; it was `claude.ai/install.sh | bash`, unpinned).
# It doesn't update itself: bump CLAUDE_CODE_VERSION in the claude stage.
COPY --from=claude /opt/claude /opt/claude
ENV PATH=/opt/claude/bin:$PATH \
    DISABLE_AUTOUPDATER=1
RUN claude --version

# dx, pinned to the `dioxus` crate's version, from the dx-cli stage.
COPY --from=dx-cli /opt/dx-cli/bin/dx /usr/local/bin/dx

# dx's tools from the dx-tools stage, and dx told to use them from PATH and
# never download (dioxus-cli's `prefer_no_downloads`, SME-413). Debian's
# wasm-opt is the one dx now runs: a binaryen bump is a deliberate change.
COPY --from=dx-tools /opt/dx-tools/bin/wasm-bindgen /opt/dx-tools/bin/esbuild /usr/local/bin/
ENV NO_DOWNLOADS=1
RUN v="$(wasm-opt --version)" && echo "$v" && echo "$v" | grep -qE '^wasm-opt version 120( |$)' \
    || { echo "error: dx runs Debian's binaryen 120 (SME-413), found: $v"; exit 1; }

WORKDIR /app

# Dev target: adds sqlx-cli for local migration/query work (slow to compile,
# not needed just to build or run the app). The same sqlx as Cargo.lock,
# from crates.io with its lockfile: crates.io's checksums, cargo's retries
# and timeouts (SME-470; it was an unpinned cargo-binstall). A cached
# layer once built.
FROM base AS dev
RUN cargo install --locked sqlx-cli@0.8.6 --no-default-features --features rustls,postgres \
    && sqlx --version

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
# dx at the version the `dioxus` crate pins — a mismatched `dx` refuses to
# build the project at all — from the dx-cli stage (SME-470).
COPY --from=dx-cli /opt/dx-cli/bin/dx /usr/local/bin/dx

WORKDIR /app

# build-env plus dx's bundle tools, the same as base's: from dx-tools, with no
# downloads (SME-413). A stage of its own so that publish-images' sandbox-image
# job, which builds build-env and never runs dx, doesn't build dx-tools.
FROM build-env AS bundle-env
COPY --from=dx-tools /opt/dx-tools/bin/wasm-bindgen /opt/dx-tools/bin/esbuild /usr/local/bin/
ENV NO_DOWNLOADS=1
RUN v="$(wasm-opt --version)" && echo "$v" && echo "$v" | grep -qE '^wasm-opt version 120( |$)' \
    || { echo "error: dx runs Debian's binaryen 120 (SME-413), found: $v"; exit 1; }

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
