# Custom sandbox image: the sandbox_agent binary IS the pod's PID 1 — no
# `sleep infinity` placeholder plus a post-hoc `pods.exec` injection (see
# docs/projects/plans/sandbox-native-environment.md). Built by
# scripts/build-sandbox-image.sh with a build context scoped to
# target/sandbox-agent/ (not the repo root, which .dockerignore excludes
# target/ from) — scripts/build-sandbox-agent.sh must have produced the
# binary there first.
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends sudo \
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

COPY sandbox_agent /usr/local/bin/sandbox_agent
RUN chmod 0755 /usr/local/bin/sandbox_agent

USER sandbox
WORKDIR /home/sandbox

ENTRYPOINT ["/usr/local/bin/sandbox_agent"]
