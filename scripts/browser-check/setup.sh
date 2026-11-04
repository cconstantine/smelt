#!/usr/bin/env bash
# Fetches Debian's headless Chromium (`chromium-headless-shell`) and the
# shared libraries it needs into a cache directory, without root and without
# touching system package state — see docs/testing.md#browser-verification
# for why this exists. Debian's, not Google's Chrome for Testing (SME-360,
# the user's choice): it's built for both amd64 and arm64, so the browser
# tier and both arches' server images run the one build. trixie keeps it on
# Chromium's stable channel through its security archive.
#
# Idempotent: safe to re-run; skips anything already fetched. Everything
# lands under .browser-check-cache/ (gitignored), never in the repo itself.
# A cache from before SME-360 (Chrome for Testing under chrome/) lacks the
# new binary, so it is simply fetched again beside it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CACHE_DIR="${BROWSER_CHECK_CACHE:-$REPO_ROOT/.browser-check-cache}"
# Debian's multiarch library directory for this machine (x86_64-linux-gnu,
# aarch64-linux-gnu); `src/headless_chrome.rs` derives the same one.
TRIPLET="$(uname -m)-linux-gnu"
# Only those two: on any other arch Debian's directory is named otherwise
# (arm-linux-gnueabihf, i386-linux-gnu, powerpc64le-linux-gnu), so the
# fetch would end in libraries "not found" rather than say why (SME-386 A7).
case "$(uname -m)" in
    x86_64 | aarch64) ;;
    *)
        echo "error: unsupported architecture $(uname -m): smelt's headless Chromium is set up for x86_64 and aarch64 only" >&2
        exit 1
        ;;
esac
LIBDIR="$CACHE_DIR/libs/usr/lib/$TRIPLET"
BIN="$CACHE_DIR/libs/usr/lib/chromium/chromium-headless-shell"

mkdir -p "$CACHE_DIR"

if [ ! -x "$BIN" ] || LD_LIBRARY_PATH="$LIBDIR:$LIBDIR/dri:${LD_LIBRARY_PATH:-}" ldd "$BIN" 2>/dev/null | grep -q "not found"; then
    echo "Fetching chromium-headless-shell and its libraries via non-root apt-get..." >&2
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

    # The package and its whole dependency closure, by name, from apt itself
    # (no Recommends), so the list follows the package on every arch and
    # every update rather than being derived by hand. The closure, not only
    # the direct dependencies: one the fetching machine has installed only
    # transitively (curl brings libbrotli1 into the chrome stage) would
    # otherwise go unfetched and missing from a slim runtime. libc6 and libgcc-s1 stay the
    # system's: a second copy on LD_LIBRARY_PATH can't be mixed with the
    # system's own loader. Naming each one matters (see --reinstall below):
    # a dependency the fetching machine happens to have installed must still
    # be fetched for a runtime that hasn't. libexpat1 and libffi8 are the two
    # that bit before (SME-62's chrome stage: python3 installs both).
    APT_OPTS=(-o Dir::State::lists="$APT_DIR/lists" -o Dir::Cache="$APT_DIR" -o Dir::Cache::archives="$CACHE_DIR/debs")
    DEPS=$(apt-cache "${APT_OPTS[@]}" depends --recurse --no-recommends --no-suggests --no-conflicts --no-breaks \
               --no-replaces --no-enhances chromium-headless-shell \
           | awk '/^ *(Pre)?Depends:/ {print $2}' | grep -v '^<' | grep -vxE 'libc6|libgcc-s1' | sort -u)
    PKGS="chromium-headless-shell libffi8 $(printf '%s ' $DEPS)"

    # --reinstall is the load-bearing flag: plain `install --print-uris`
    # silently omits any package dpkg already considers installed, so a
    # fetcher that happens to have some of PKGS system-wide — the Docker
    # chrome stage does, python3 pulls in libexpat1 and libffi8, exactly
    # the two the original PKGS derivation missed — "fetches" them into
    # nothing, the cache is incomplete, and every check on that machine
    # still passes because the system has what the cache lacks. --reinstall
    # forces a URL for every named package regardless of dpkg state, while
    # keeping apt's name resolution (libatk1.0-0 → its t64 package on
    # trixie) and its fetching of not-yet-installed dependencies, both of
    # which `apt-get download` does not do.
    URLS=$(apt-get -o Dir::State::lists="$APT_DIR/lists" \
                    -o Dir::Cache="$APT_DIR" \
                    -o Dir::Cache::archives="$CACHE_DIR/debs" \
                    -o APT::Install-Recommends=false \
                    -o APT::Install-Suggests=false \
                    --print-uris -qq --reinstall install $PKGS | awk -F"'" '{print $2}')

    if [ -z "$URLS" ]; then
        echo "error: apt-get --print-uris returned no package URLs. The update above fetched a real index (see the listing before this), so re-running the same install non-quietly to see apt's actual reasoning (already satisfied? unavailable? something else):" >&2
        apt-get -o Dir::State::lists="$APT_DIR/lists" \
                -o Dir::Cache="$APT_DIR" \
                -o Dir::Cache::archives="$CACHE_DIR/debs" \
                -o APT::Install-Recommends=false \
                -o APT::Install-Suggests=false \
                --print-uris --reinstall install $PKGS >&2 2>&1 || true
        echo "--- dpkg status for these packages (system-wide, not our custom cache) ---" >&2
        dpkg -l $PKGS >&2 2>&1 || true
        exit 1
    fi

    # ~120 packages for Debian's Chromium (the old Chrome-for-Testing list was
    # ~15): four at a time, each retried, and -f so an error page never
    # lands as a .deb. Eight at once had deb.debian.org refusing
    # connections ("Failed to connect … port 80") partway through.
    (cd "$CACHE_DIR/debs" && printf '%s\n' "$URLS" \
        | xargs -P 4 -n 1 curl -fsSL --retry 3 --retry-connrefused --retry-delay 2 -O)

    # Only this run's .debs, by the names curl -O saved them under (each
    # URL's last part): debs/ keeps every earlier run's too, and unpacking
    # them all let an older build overwrite a newer one wherever it sorted
    # later (2.4-1+deb13u9 after +deb13u10; SME-386 A5).
    while IFS= read -r url; do
        dpkg-deb -x "$CACHE_DIR/debs/${url##*/}" "$CACHE_DIR/libs"
    done <<< "$URLS"

    # libpulse finds its private libpulsecommon through an absolute path to
    # the system's pulseaudio/ directory, which the cache's copy can't
    # follow: put copies beside the rest, where LD_LIBRARY_PATH reaches them.
    if [ -d "$LIBDIR/pulseaudio" ]; then
        cp -n "$LIBDIR"/pulseaudio/*.so "$LIBDIR"/ 2>/dev/null || true
    fi

    # The fetch was the whole point — confirm it produced a libdir that
    # actually resolves everything Chrome links. Before this, a fetch that
    # missed a library (a new Chrome dependency, or one the derivation
    # machine had installed system-wide) was only noticed at launch, in
    # whatever environment happened to have the gap.
    STILL_MISSING=$(LD_LIBRARY_PATH="$LIBDIR:$LIBDIR/dri:${LD_LIBRARY_PATH:-}" \
        ldd "$BIN" 2>/dev/null | grep "not found" || true)
    if [ -n "$STILL_MISSING" ]; then
        echo "error: shared libraries still missing after the fetch:" >&2
        printf '%s\n' "$STILL_MISSING" >&2
        echo "Map each to its Debian package (https://packages.debian.org/trixie/)" >&2
        echo "and add it to PKGS in this script." >&2
        exit 1
    fi
else
    echo "Shared libraries already present, skipping." >&2
fi

echo "Ready:" >&2
echo "  binary:  $BIN" >&2
echo "  libdir:  $LIBDIR" >&2
