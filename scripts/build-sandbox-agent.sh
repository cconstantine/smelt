#!/bin/sh
# Builds the sandbox_agent binary and copies it to a fixed,
# profile-independent path (target/sandbox-agent/sandbox_agent) that
# src/sandbox.rs's `include_bytes!` references. Must run before building
# the main server binary — see docs/setup.md and
# docs/projects/plans/sandbox-terminal.md's "Build ordering."
#
# Deliberately a separate script rather than a build.rs: a build.rs that
# itself shells out to `cargo build --bin sandbox_agent` for a *second*
# binary in the *same* package risks recursively re-invoking its own
# package's build.rs — a documented two-command sequence sidesteps that
# entirely, at the cost of the developer having to remember to run it.
set -eu

cd "$(dirname "$0")/.."

cargo build --bin sandbox_agent --features server "$@"

# Same toolchain/target as the main build (see the plan) — always the
# debug or release dir matching whatever profile flag was passed through.
PROFILE_DIR=debug
for arg in "$@"; do
    if [ "$arg" = "--release" ]; then
        PROFILE_DIR=release
    fi
done

mkdir -p target/sandbox-agent
cp "target/$PROFILE_DIR/sandbox_agent" target/sandbox-agent/sandbox_agent
echo "sandbox_agent built and copied to target/sandbox-agent/sandbox_agent"
