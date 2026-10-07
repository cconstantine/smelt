#!/usr/bin/env bash
# The checks every commit must pass (docs/development-process.md, "Every
# commit builds for both targets"): the web build and the server build,
# each with no warnings, no unexplained expect() outside tests
# (scripts/lint-expects), and the server tests, run against this working
# tree's own sandbox image (SANDBOX_IMAGE, see scripts/sandbox-image-ref).
# Exits non-zero on any failure, so it can gate a commit:
#
#   scripts/check.sh && git commit ...
#
# The browser tier (`cargo test --features "server browser-test" --
# --ignored --test-threads=1`) isn't run here: it needs a built web
# bundle and a real browser, and belongs before calling a change done.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== web build (wasm32)"
web_output=$(cargo check --no-default-features --features web --target wasm32-unknown-unknown 2>&1) || {
    echo "$web_output"
    exit 1
}
if grep -qE '^(warning|error)' <<<"$web_output"; then
    echo "$web_output" | grep -E '^(warning|error)' -A8
    echo "the web build has warnings or errors"
    exit 1
fi

# The server binary on its own, not just the test build: code only a
# `#[cfg(test)]` import makes compile passes every test and still breaks
# the binary (SME-33).
echo "== server build"
server_output=$(cargo build --features server 2>&1) || {
    echo "$server_output" | grep -E '^(warning|error)' -A8
    echo "the server build failed"
    exit 1
}
if grep -qE '^(warning|error)' <<<"$server_output"; then
    echo "$server_output" | grep -E '^(warning|error)' -A8
    echo "the server build has warnings"
    exit 1
fi

# No new expect() outside tests without a reason (SME-95).
lint_output=$(scripts/lint-expects 2>&1) || {
    echo "$lint_output" | grep -E '^(warning|error)' -A8
    echo "an expect() in production code: handle the error, or say why it can't fail (scripts/lint-expects)"
    exit 1
}

# The tests' pods run this working tree's own agent, from the image named
# after its sources (SME-102), not the shared `:latest` the dev server uses.
SANDBOX_IMAGE=${SANDBOX_IMAGE:-$(scripts/sandbox-image-ref)}
export SANDBOX_IMAGE

# Before the tests: the real-cluster ones all fail the same way on a
# missing sandbox image, and this says so once (SME-53).
echo "== cluster ($SANDBOX_IMAGE)"
scripts/cluster-doctor

echo "== server tests"
# Kept, and overwritten by the next run: this script deletes no files, so a
# session can run it unattended (SME-107).
mkdir -p target/check
log=target/check/server-tests.log
if ! cargo test --features server >"$log" 2>&1; then
    grep -E 'FAILED|panicked|^error' -A3 "$log" | head -40
    echo "server tests failed (full log: $log)"
    exit 1
fi
grep -E '^test result' "$log" | tail -1
echo "all checks passed"
