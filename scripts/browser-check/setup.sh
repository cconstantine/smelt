#!/usr/bin/env bash
# Downloads a headless Chrome-for-Testing binary and the shared libraries
# it needs, without root and without touching system package state — see
# docs/testing.md#browser-verification for why this exists.
#
# Idempotent: safe to re-run; skips anything already fetched. Everything
# lands under .browser-check-cache/ (gitignored), never in the repo itself.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CACHE_DIR="${BROWSER_CHECK_CACHE:-$REPO_ROOT/.browser-check-cache}"
CHROME_DIR="$CACHE_DIR/chrome"
LIBDIR="$CACHE_DIR/libs/usr/lib/x86_64-linux-gnu"
BIN="$CHROME_DIR/chrome-headless-shell-linux64/chrome-headless-shell"

mkdir -p "$CACHE_DIR"

if [ ! -x "$BIN" ]; then
    echo "Downloading chrome-headless-shell (Stable channel)..." >&2
    URL=$(curl -sL https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json \
        | python3 -c "
import json, sys
d = json.load(sys.stdin)
for dl in d['channels']['Stable']['downloads']['chrome-headless-shell']:
    if dl['platform'] == 'linux64':
        print(dl['url'])
        break
")
    mkdir -p "$CHROME_DIR"
    curl -sL "$URL" -o "$CHROME_DIR/chs.zip"
    unzip -q -o "$CHROME_DIR/chs.zip" -d "$CHROME_DIR"
    rm "$CHROME_DIR/chs.zip"
else
    echo "chrome-headless-shell already present, skipping download." >&2
fi

if [ ! -f "$LIBDIR/libnspr4.so" ]; then
    echo "Fetching missing shared libraries via non-root apt-get..." >&2
    APT_DIR="$CACHE_DIR/apt"
    mkdir -p "$APT_DIR/lists/partial" "$APT_DIR/archives/partial" "$CACHE_DIR/debs" "$CACHE_DIR/libs"

    # A scratch Dir::State::lists/Dir::Cache lets `apt-get update` and
    # `--print-uris`/download work entirely as the current user — nothing
    # here touches /var/lib/dpkg or /var/cache/apt. Not -qq: this has
    # silently "succeeded" (exit 0) while fetching a usable index for
    # zero packages in CI at least twice — visible output here is the
    # only way to see why until that's understood.
    apt-get -o Dir::State::lists="$APT_DIR/lists" \
            -o Dir::Cache="$APT_DIR" \
            -o Dir::Cache::archives="$CACHE_DIR/debs" \
            update
    echo "--- apt sources in use ---" >&2
    cat /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list >&2 2>&1 || true
    echo "--- fetched lists dir ---" >&2
    ls -la "$APT_DIR/lists" >&2 || true

    # This list was derived by running `ldd` against chrome-headless-shell
    # and mapping each "not found" .so to its owning Debian package
    # (trixie). --no-install-recommends keeps this to just the ~35 actual
    # library packages, not systemd/dbus-daemon/etc. pulled in as
    # Recommends.
    PKGS="libnspr4 libnss3 libatk1.0-0 libatk-bridge2.0-0 libdbus-1-3 \
libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 \
libxkbcommon0 libasound2 libatspi2.0-0"

    URLS=$(apt-get -o Dir::State::lists="$APT_DIR/lists" \
                    -o Dir::Cache="$APT_DIR" \
                    -o Dir::Cache::archives="$CACHE_DIR/debs" \
                    -o APT::Install-Recommends=false \
                    -o APT::Install-Suggests=false \
                    --print-uris -qq install $PKGS | awk -F"'" '{print $2}')

    if [ -z "$URLS" ]; then
        echo "error: apt-get --print-uris returned no package URLs — the redirected 'apt-get update' above likely failed to fetch a usable index (network issue, mirror hiccup, or a real apt error masked by -qq)." >&2
        exit 1
    fi

    (cd "$CACHE_DIR/debs" && printf '%s\n' "$URLS" | xargs -P 8 -n 1 curl -sL -O)

    for deb in "$CACHE_DIR"/debs/*.deb; do
        dpkg-deb -x "$deb" "$CACHE_DIR/libs"
    done
else
    echo "Shared libraries already present, skipping." >&2
fi

echo "Ready:" >&2
echo "  binary:  $BIN" >&2
echo "  libdir:  $LIBDIR" >&2
