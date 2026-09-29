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
set -eu

cd "$(dirname "$0")/.."

# Taken before building, so an edit made while this runs doesn't pass for
# what was built. Recorded once both images are in (see scripts/cluster-doctor).
SOURCES_HASH=$(scripts/agent-sources-hash)

scripts/build-sandbox-agent.sh "$@"

docker build -f docker/sandbox/Dockerfile -t smelt-sandbox:latest target/sandbox-agent/

mkdir -p target/sandbox-image
TAR_PATH=target/sandbox-image/smelt-sandbox.tar
docker save -o "$TAR_PATH" smelt-sandbox:latest

cargo run --bin sandbox_image_import --features server -- "$TAR_PATH"

# The pod's Docker sidecar (SME-33) runs dockerd from docker:dind. Delivered
# the same way, so pods start without a pull and CI doesn't depend on
# Docker Hub at pod-start time. Keep the tag in step with
# src/sandbox.rs's default_docker_image.
DOCKER_IMAGE=docker:29-dind
DOCKER_TAR_PATH=target/sandbox-image/docker-dind.tar
docker pull -q "$DOCKER_IMAGE"
docker save -o "$DOCKER_TAR_PATH" "$DOCKER_IMAGE"
cargo run --bin sandbox_image_import --features server -- "$DOCKER_TAR_PATH"

echo "$SOURCES_HASH" > target/sandbox-image/agent-sources.sha256

echo "smelt-sandbox:latest and $DOCKER_IMAGE built and imported into the cluster"
