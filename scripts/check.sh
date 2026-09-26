#!/usr/bin/env bash
# The checks every commit must pass (docs/development-process.md, "Every
# commit builds for both targets"): the web build, with no warnings, and
# the server tests. Exits non-zero on any failure, so it can gate a
# commit:
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

echo "== server tests"
log=$(mktemp)
trap 'rm -f "$log"' EXIT
if ! cargo test --features server >"$log" 2>&1; then
    grep -E 'FAILED|panicked|^error' -A3 "$log" | head -40
    echo "server tests failed (full log: rerun without scripts/check.sh)"
    exit 1
fi
grep -E '^test result' "$log" | tail -1
echo "all checks passed"
