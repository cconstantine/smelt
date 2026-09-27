# Custom sandbox image: the sandbox_agent binary IS the pod's PID 1 — no
# `sleep infinity` placeholder plus a post-hoc `pods.exec` injection (see
# SME-17). Built by
# scripts/build-sandbox-image.sh with a build context scoped to
# target/sandbox-agent/ (not the repo root, which .dockerignore excludes
# target/ from) — scripts/build-sandbox-agent.sh must have produced the
# binary there first.
FROM debian:trixie-slim

# The everyday tools most coding tasks reach for first. Without them a
# first task started by failing and installing them (SME-41 D10);
# anything else is still one `sudo apt-get install` away.
RUN apt-get update && apt-get install -y --no-install-recommends \
        sudo ca-certificates curl git python3 \
    && rm -rf /var/lib/apt/lists/*

# The Docker client, compose and buildx, from Docker's own apt repo. The
# daemon runs in the pod's `docker` sidecar, not here (SME-33): this
# container stays unprivileged. Pinned to the sidecar's major version
# (docker:29-dind, see src/sandbox.rs's default_docker_image).
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && echo "deb [signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian trixie stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update && apt-get install -y --no-install-recommends \
        'docker-ce-cli=5:29.*' docker-buildx-plugin docker-compose-plugin \
    && rm -rf /var/lib/apt/lists/*

# A regular, pre-created user — commands run unprivileged by default,
# matching how a person actually works day to day. NOPASSWD sudo is what
# preserves the ability to reach for root when a command genuinely needs
# it, without any agent-level privilege-drop dance: the agent itself just
# runs as `sandbox` (via USER below), and every shell it spawns inherits
# that same identity by ordinary fork/exec.
RUN useradd -m -s /bin/bash sandbox \
    && echo "sandbox ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/sandbox \
    && chmod 0440 /etc/sudoers.d/sandbox

# The sidecar's dockerd gives this group its socket (`--group`, see
# src/sandbox.rs's DOCKER_GID), so `docker` works without sudo. The GID is
# fixed because the sidecar has no /etc/group entry to look it up in.
RUN groupadd -g 2375 docker && usermod -aG docker sandbox
ENV DOCKER_HOST=unix:///run/docker-sock/docker.sock

# Shared with the Docker sidecar, so a bind mount from here is the same
# files in a container (SME-33). Terminals start here: the agent's working
# directory is the WORKDIR below, and every shell inherits it. In a pod it
# is an emptyDir mounted over this directory.
RUN mkdir /workspace && chown sandbox:sandbox /workspace

COPY sandbox_agent /usr/local/bin/sandbox_agent
RUN chmod 0755 /usr/local/bin/sandbox_agent

USER sandbox
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/sandbox_agent"]
