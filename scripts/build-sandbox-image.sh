#!/bin/sh
# Builds the custom sandbox image (docker/sandbox/Dockerfile) and delivers
# it to the cluster with no registry involved — see
# SME-17's Phase 1. A manual
# step, not wired into `docker compose up` itself: unlike
# build-sandbox-agent.sh, this needs a live cluster (DOCKER_HOST,
# KUBECONFIG) to do anything at all, so it can only run *after* the
# compose stack (including k3s-bootstrap) is up. Run it from inside the
# `smelt` container: `docker compose exec smelt scripts/build-sandbox-image.sh`.
#
# Two distinct halves — build+tag, then deliver — on purpose: a later CI
# job could swap `docker push` to a real registry in for the deliver half
# without touching the build+tag half. Not built now, just not designed
# against. See docs/setup.md.
#
# The image is named after the agent sources it's built from,
# `smelt-sandbox:src-<hash>` (scripts/sandbox-image-ref; SME-102), which is
# what check.sh, browser-tier, check-server and CI run their tests against.
# `smelt-sandbox:latest`, what a server with no SANDBOX_IMAGE uses (the dev
# server), moves only with --latest: run that after pulling a change to the
# agent, not to test a branch. Both tags go in one tar and one import, so
# they can't end up pointing at different builds. Other arguments go to
# scripts/build-sandbox-agent.sh (e.g. --release).
set -eu

cd "$(dirname "$0")/.."

LATEST=no
n=$#
while [ "$n" -gt 0 ]; do
    arg=$1
    shift
    n=$((n - 1))
    if [ "$arg" = "--latest" ]; then
        LATEST=yes
    else
        set -- "$@" "$arg"
    fi
done

# Taken before building, so an edit made while this runs doesn't pass for
# what was built.
REF=$(scripts/sandbox-image-ref)
TAG=${REF#docker.io/library/}

scripts/build-sandbox-agent.sh "$@"

docker build -f docker/sandbox/Dockerfile -t "$TAG" target/sandbox-agent/
TAGS=$TAG
if [ "$LATEST" = yes ]; then
    docker tag "$TAG" smelt-sandbox:latest
    TAGS="$TAG smelt-sandbox:latest"
fi

mkdir -p target/sandbox-image
TAR_PATH=target/sandbox-image/smelt-sandbox.tar
# shellcheck disable=SC2086 # one or two tags, split on purpose
docker save -o "$TAR_PATH" $TAGS

cargo run --bin sandbox_image_import --features server -- "$TAR_PATH"

# The pod's Docker sidecar (SME-33) runs dockerd from docker:dind. Delivered
# the same way, so pods start without a pull and CI doesn't depend on
# Docker Hub at pod-start time. Keep the tag in step with
# src/sandbox/spec.rs's default_docker_image.
DOCKER_IMAGE=docker:29-dind
DOCKER_TAR_PATH=target/sandbox-image/docker-dind.tar
docker pull -q "$DOCKER_IMAGE"
docker save -o "$DOCKER_TAR_PATH" "$DOCKER_IMAGE"
cargo run --bin sandbox_image_import --features server -- "$DOCKER_TAR_PATH"

echo "$TAGS and $DOCKER_IMAGE built and imported into the cluster"
