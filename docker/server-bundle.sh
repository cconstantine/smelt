#!/usr/bin/env bash
# The Dockerfile's server-build stage: the release server binary and its web
# bundle, laid out in /app/image/smelt for the runtime stage. A script so
# that scripts/test-dx-offline runs exactly what the stage runs (SME-413).
set -euo pipefail

# dx estimates its progress display with `cargo +nightly rustc --unit-graph`;
# this image has stable only, and rustup's shim answers a missing toolchain by
# installing it from the network with no timeout (SME-457's hang). The
# estimate is cosmetic and dx falls back on a failed one (dioxus-cli 0.7.9's
# get_unit_count_estimate), so make it fail fast, as scripts/ci-web-bundle
# does (SME-413 review L1).
export RUSTUP_AUTO_INSTALL=0

# --debug-symbols false: dioxus-cli 0.7.9 defaults it to true, which keeps
# the wasm's DWARF and runs wasm-opt with --debuginfo, and Debian's wasm-opt
# 120 aborts (SIGABRT) on that DWARF. Off, wasm-opt gets --strip-debug. The
# published wasm drops only its DWARF (read only by a browser DWARF
# extension); the name section is dx's keep_names, off either way (SME-413).
dx bundle --platform web --release --debug-symbols false 2>&1 | tee /tmp/dx-bundle.log

# dx only logs a wasm-opt that fails ("wasm-opt failed with status code
# ...", dioxus-cli 0.7.9's wasm_opt.rs) and ships the unoptimised wasm. Now
# that dx runs Debian's wasm-opt rather than its own download (SME-413),
# a wasm-opt that refuses our module fails the image build instead.
if grep -q 'wasm-opt failed' /tmp/dx-bundle.log; then
    echo "error: dx's wasm-opt failed and dx shipped the unoptimised wasm (SME-413); see its output above" >&2
    exit 1
fi

# dx prints where it put things, but the path has moved between dx versions
# and isn't something to bake into this file. So: find the web bundle, and
# take the executable beside it. Loudly give up with what's actually there if
# that doesn't work, rather than building an image that can only fail at
# startup. (`|| true` on the finds: there's usually no dist, and pipefail
# would otherwise end the script on find's status, silently.)
public="$(find target/dx dist -type d -name public 2>/dev/null | head -n1 || true)"
test -n "$public" || { echo "error: no web bundle under target/dx or dist — see dx's output above"; ls -R target/dx 2>/dev/null | head -40; exit 1; }
bindir="$(dirname "$public")"
server="$(find "$bindir" -maxdepth 1 -type f -perm -u+x 2>/dev/null | head -n1 || true)"
test -n "$server" || { echo "error: no executable beside $public"; ls -l "$bindir"; exit 1; }
install -d /app/image/smelt
cp -a "$bindir/." /app/image/smelt/
mv "/app/image/smelt/$(basename "$server")" /app/image/smelt/smelt
echo "server image: $server (renamed smelt) with the bundle from $public"
