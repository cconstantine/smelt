#!/bin/sh
# Builds the custom sandbox image (docker/sandbox/Dockerfile) and delivers
# it to the cluster with no registry involved — see
# docs/projects/plans/sandbox-native-environment.md's Phase 1. A manual
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

scripts/build-sandbox-agent.sh "$@"

docker build -f docker/sandbox/Dockerfile -t smelt-sandbox:latest target/sandbox-agent/

mkdir -p target/sandbox-image
TAR_PATH=target/sandbox-image/smelt-sandbox.tar
docker save -o "$TAR_PATH" smelt-sandbox:latest

cargo run --bin sandbox_image_import --features server -- "$TAR_PATH"

echo "smelt-sandbox:latest built and imported into the cluster"
