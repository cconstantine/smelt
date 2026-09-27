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
        sudo ca-certificates curl git openssh-client python3 \
    && rm -rf /var/lib/apt/lists/*

# Git over SSH (SME-32). smelt writes the user's keys, an SSH config
# offering them and the commit identity into /etc/smelt when a pod starts
# (src/git.rs's pod_git_files); these two includes make ssh and git read
# them, leaving the home directory (maybe a volume of the user's own)
# alone. A user's own ~/.ssh/config or ~/.gitconfig still wins.
RUN mkdir -m 0755 /etc/smelt \
    && touch /etc/smelt/ssh_config /etc/smelt/gitconfig \
    && echo "Include /etc/smelt/ssh_config" > /etc/ssh/ssh_config.d/smelt.conf \
    && git config --system include.path /etc/smelt/gitconfig

# The published host keys of the common git hosts, so the first push
# isn't a trust-on-first-use guess. From https://api.github.com/meta,
# https://bitbucket.org/site/ssh and
# https://docs.gitlab.com/user/gitlab_com/ (fetched 2026-09-27).
RUN printf '%s\n' \
        "github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl" \
        "github.com ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=" \
        "github.com ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCj7ndNxQowgcQnjshcLrqPEiiphnt+VTTvDP6mHBL9j1aNUkY4Ue1gvwnGLVlOhGeYrnZaMgRK6+PKCUXaDbC7qtbW8gIkhL7aGCsOr/C56SJMy/BCZfxd1nWzAOxSDPgVsmerOBYfNqltV9/hWCqBywINIR+5dIg6JTJ72pcEpEjcYgXkE2YEFXV1JHnsKgbLWNlhScqb2UmyRkQyytRLtL+38TGxkxCflmO+5Z8CSSNY7GidjMIZ7Q4zMjA2n1nGrlTDkzwDCsw+wqFPGQA179cnfGWOWRVruj16z6XyvxvjJwbz0wQZ75XK5tKSb7FNyeIEs4TT4jk+S4dhPeAUC5y+bDYirYgM4GC7uEnztnZyaVWQ7B381AK4Qdrwt51ZqExKbQpTUNn+EjqoTwvqNj4kqx5QUCI0ThS/YkOxJCXmPUWZbhjpCg56i+2aB6CmK2JGhn57K5mj0MNdBXA4/WnwH6XoPWJzK5Nyu2zB3nAZp+S5hpQs+p1vN1/wsjk=" \
        "bitbucket.org ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDQeJzhupRu0u0cdegZIa8e86EG2qOCsIsD1Xw0xSeiPDlCr7kq97NLmMbpKTX6Esc30NuoqEEHCuc7yWtwp8dI76EEEB1VqY9QJq6vk+aySyboD5QF61I/1WeTwu+deCbgKMGbUijeXhtfbxSxm6JwGrXrhBdofTsbKRUsrN1WoNgUa8uqN1Vx6WAJw1JHPhglEGGHea6QICwJOAr/6mrui/oB7pkaWKHj3z7d1IC4KWLtY47elvjbaTlkN04Kc/5LFEirorGYVbt15kAUlqGM65pk6ZBxtaO3+30LVlORZkxOh+LKL/BvbZ/iRNhItLqNyieoQj/uh/7Iv4uyH/cV/0b4WDSd3DptigWq84lJubb9t/DnZlrJazxyDCulTmKdOR7vs9gMTo+uoIrPSb8ScTtvw65+odKAlBj59dhnVp9zd7QUojOpXlL62Aw56U4oO+FALuevvMjiWeavKhJqlR7i5n9srYcrNV7ttmDw7kf/97P5zauIhxcjX+xHv4M=" \
        "bitbucket.org ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBPIQmuzMBuKdWeF4+a2sjSSpBK0iqitSQ+5BM9KhpexuGt20JpTVM7u5BDZngncgrqDMbWdxMWWOGtZ9UgbqgZE=" \
        "bitbucket.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIazEu89wgQZ4bqs3d63QSMzYVa0MuJ2e2gKTKqu+UUO" \
        "gitlab.com ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFSMqzJeV9rUzU4kWitGjeR4PWSa29SPqJ1fVkhtj3Hw9xjLVXVYrU9QlYWrOLXBpQ6KWjbjTDTdDkoohFzgbEY=" \
        "gitlab.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAfuCHKVTjquxvt6CM6tdG4SLp1Btn/nOeHHE5UOzRdf" \
        "gitlab.com ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCsj2bNKTBSpIYDEGk9KxsGh3mySTRgMtXL583qmBpzeQ+jqCMRgBqB98u3z++J1sKlXHWfM9dyhSevkMwSbhoR8XIq/U0tCNyokEi/ueaBMCvbcTHhO7FcwzY92WK4Yt0aGROY5qX2UKSeOvuP4D6TPqKF1onrSzH9bx9XUf2lEdWT/ia1NEKjunUqu1xOB/StKDHMoX4/OKyIzuS0q/T1zOATthvasJFoPrAjkohTyaDUz2LN5JoH839hViyEG82yB+MjcFV5MU3N1l1QL3cVUCh93xSaua1N85qivl+siMkPGbO5xR/En4iEY6K2XPASUEMaieWVNTRCtJ4S8H+9" \
        > /etc/ssh/ssh_known_hosts

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
# uid/gid pinned: the Docker sidecar hands /workspace to 1000:1000 (see
# src/sandbox.rs's SANDBOX_OWNER), knowing no names from this image.
RUN groupadd -g 1000 sandbox \
    && useradd -m -u 1000 -g 1000 -s /bin/bash sandbox \
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

# smelt writes /etc/smelt as the sandbox user (see above).
RUN chown -R sandbox:sandbox /etc/smelt

COPY sandbox_agent /usr/local/bin/sandbox_agent
RUN chmod 0755 /usr/local/bin/sandbox_agent

USER sandbox
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/sandbox_agent"]
