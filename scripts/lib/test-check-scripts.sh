# Shared by scripts/test-check-server, test-check-server-scratch,
# test-check-server-guards, test-check-server-db-inputs,
# test-check-server-ports, test-cluster-lock and test-gate-scripts
# (SME-163; they were one script, test-check-scripts):
# the stubs and helpers their cases use. Sourced, never run. Each script
# keeps its files, the stubs and the locks under its own
# target/<script name>/, so no two share a lock. Needs no cluster or
# database: cargo, dx, sqlx and the image check are stubs (dx serve's stub
# really listens, on a port of the script's own, SME-175), and every lock
# lives under target/<script name>/locks (SMELT_LOCK_DIR), never the real
# one. Claude Code's safety check refuses a command whose
# scripts remove files, so everything they write is kept, overwritten on
# the next run.

set -uo pipefail
cd "$(dirname "$0")/.."

# ps and pgrep (Debian's procps) are used by the check-server suites' cases
# and by check-server itself; without them dozens of cases fail as if the
# code were wrong (SME-267). test-cluster-lock and test-gate-scripts use
# neither, so they aren't stopped. Each is checked on its own:
# `command -v ps pgrep` succeeds when either one is found.
case $(basename "$0") in
test-check-server*)
    for tool in ps pgrep; do
        command -v "$tool" >/dev/null || {
            echo "$(basename "$0") needs ps and pgrep, and $tool is missing (Debian: sudo apt-get install procps)" >&2
            exit 1
        }
    done
    ;;
esac

failures=0
fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}


dir=$PWD/target/$(basename "$0")
mkdir -p "$dir/bin"

# Every lock these scripts take goes here, never the real cluster lock
# (SME-100): another session's gate may hold that one.
export SMELT_LOCK_DIR=$dir/locks
mkdir -p "$SMELT_LOCK_DIR"
lock=$SMELT_LOCK_DIR/smelt-cluster.lock
lock_log=$SMELT_LOCK_DIR/smelt-cluster-lock.log
: >"$lock_log"
# Whether the cluster lock is free: a fresh open and a non-blocking flock,
# which fails while any other open of the file holds it.
lock_is_free() { flock -n "$lock" true; }
# Waits up to 10 s for the cluster lock to be taken.
wait_until_held() {
    for _ in $(seq 1 100); do
        lock_is_free || return 0
        sleep 0.1
    done
    return 1
}
# Whether a process id names a live process. kill -0 succeeds on a zombie,
# and nothing in a container reaps the orphans a killed run leaves behind,
# so a check that asks whether something still runs has to read the state:
# /proc/<pid>/stat names it after the command name, which can hold spaces
# and ')'.
pid_is_live() {
    local state
    [ -r "/proc/$1/stat" ] || return 1
    state=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f1)
    [ -n "$state" ] && [ "$state" != Z ]
}
# Whether a live process is in process group $1, for the same reason: pgrep
# counts a zombie too.
group_is_live() {
    local pid
    for pid in $(pgrep -g "$1" 2>/dev/null); do
        pid_is_live "$pid" && return 0
    done
    return 1
}

# Each stub below that the gate scripts call appends a line to $CALLS (when
# set): its arguments, then "locked" or "unlocked", whether another process
# held the cluster lock at the time (SME-100).
cat >"$dir/bin/lock-state" <<'EOF'
#!/usr/bin/env bash
flock -n "$SMELT_LOCK_DIR/smelt-cluster.lock" true && echo unlocked || echo locked
EOF
# cargo: records each call. A test run (not --no-run) prints a result line.
# CARGO_FAIL: a pattern; a call whose arguments match it fails.
cat >"$dir/bin/cargo" <<'EOF'
#!/usr/bin/env bash
echo "cargo $* $(lock-state)" >>"$CALLS"
if [ -n "${CARGO_FAIL:-}" ] && [[ "$*" =~ $CARGO_FAIL ]]; then
    echo "error: stub cargo told to fail"
    exit 101
fi
if [ "$1" = test ] && [[ " $* " != *" --no-run "* ]]; then
    echo "test result: ok. 1 passed; 0 failed"
fi
exit 0
EOF
# The real python3 and curl, found before any stub directory is on PATH
# (test-cluster-lock puts a python3 stub in its own bin).
export STUB_PYTHON3 STUB_REAL_CURL
STUB_PYTHON3=$(command -v python3)
STUB_REAL_CURL=$(command -v curl)
# An HTTP server answering 200 to every GET (SME-175): the dx stub's server,
# and the cases' foreign listeners.
#   http-200.py HOST PORT [--own-group] [--ready FILE] [--delay SECONDS]
#                         [--status CODE]
# --own-group puts it in a process group of its own (a foreign listener),
# unless it already leads one: setsid fails for a group leader, as a
# background job is under job control. It also closes every descriptor
# above 2 it inherited, so it never holds a lock its starter held (review
# 1: a run under an outer flock, or a hook inside check-server's start). --ready writes its pid to FILE once
# it listens; --delay waits before binding; --status answers CODE instead
# of 200 (a server still building answers 500). A bind that fails ends it, as
# a taken port ends a server. It ends itself after 600 s, so a run killed
# before its cleanup leaves nothing behind for long.
export STUB_HTTP_200=$dir/http-200.py
cat >"$STUB_HTTP_200" <<'EOF'
import http.server, os, signal, socket, sys, time
host, port, args = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
if "--own-group" in args:
    if os.getpgrp() != os.getpid():
        os.setsid()
    os.closerange(3, os.sysconf("SC_OPEN_MAX"))
signal.alarm(600)
if "--delay" in args:
    time.sleep(float(args[args.index("--delay") + 1]))
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(int(args[args.index("--status") + 1]) if "--status" in args else 200)
        self.send_header("Content-Length", "3")
        self.end_headers()
        self.wfile.write(b"ok\n")
    def log_message(self, *_):
        pass
class Server(http.server.HTTPServer):
    address_family = socket.AF_INET6 if ":" in host else socket.AF_INET
    # Never SO_REUSEPORT, which some Python releases turned on for
    # HTTPServer: two stubs on one port must collide, as dx does (review 2).
    allow_reuse_port = False
server = Server((host, port), Handler)
if "--ready" in args:
    with open(args[args.index("--ready") + 1], "w") as ready:
        ready.write(str(os.getpid()))
server.serve_forever()
EOF
# dx serve: serves its --port until stopped, like the real one, with an
# HTTP server on 0.0.0.0 answering 200 (SME-175; it was a `sleep`, which
# only a curl stub that always succeeded made look up). It first records
# the log settings it was started with (the server it runs inherits them)
# and writes a line of its own to the log. DX_NO_LISTEN=1 keeps it alive
# without binding; DX_BIND_DELAY=<s> waits before the bind; DX_STATUS
# answers that code instead of 200. dx build records the call and ends.
cat >"$dir/bin/dx" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = build ]; then
    echo "dx $* $(lock-state)" >>"$CALLS"
    exit 0
fi
echo "stub dx started as $$"
printf 'RUST_LOG=%s\nNO_COLOR=%s\n' "${RUST_LOG-<unset>}" "${NO_COLOR-<unset>}" >"$DX_ENV.tmp"
mv "$DX_ENV.tmp" "$DX_ENV"
port=""
while [ $# -gt 0 ]; do
    [ "$1" = --port ] && port=$2
    shift
done
[ -n "${DX_NO_LISTEN:-}" ] && exec sleep 600
exec "$STUB_PYTHON3" -I "$STUB_HTTP_200" 0.0.0.0 "$port" --delay "${DX_BIND_DELAY:-0}" --status "${DX_STATUS:-200}"
EOF
# sqlx: records each call, one per line, and to $ORDER when set (with the
# cleanup stub's lines, so a case can check their order).
cat >"$dir/bin/sqlx" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$SQLX_CALLS"
[ -n "${ORDER:-}" ] && echo "sqlx $*" >>"$ORDER"
[ -n "${CALLS:-}" ] && echo "sqlx $* $(lock-state)" >>"$CALLS"
exit 0
EOF
# curl: the real one (SME-175: a stub that always succeeded was the bug's
# own shape, any answer on the port passing for the server's). CURL_HOOK,
# when set, runs first, so a case can change who listens on the port
# between check-server's checks and its request.
cat >"$dir/bin/curl" <<'EOF'
#!/usr/bin/env bash
[ -n "${CURL_HOOK:-}" ] && "$CURL_HOOK"
exec "$STUB_REAL_CURL" "$@"
EOF
# The image check: the cluster has it.
cat >"$dir/bin/import" <<'EOF'
#!/usr/bin/env bash
[ -n "${CALLS:-}" ] && echo "import $(lock-state)" >>"$CALLS"
exit 0
EOF
# The instance cleanup (SME-129): records each call with the KUBECONFIG it
# got, and to $ORDER when set. CLEANUP_FAIL makes it fail (a guard, a
# timeout, an unreachable cluster); CLEANUP_SLEEP makes it slow, so a case
# can hold the scratch lock while it runs.
cat >"$dir/bin/cleanup" <<'EOF'
#!/usr/bin/env bash
printf 'KUBECONFIG=%s %s\n' "${KUBECONFIG-<unset>}" "$*" >>"$CLEANUP_CALLS"
[ -n "${ORDER:-}" ] && echo "cleanup $*" >>"$ORDER"
[ -n "${CLEANUP_SLEEP:-}" ] && sleep "$CLEANUP_SLEEP"
[ -n "${CLEANUP_FAIL:-}" ] && exit 3
exit 0
EOF
chmod +x "$dir/bin/lock-state" "$dir/bin/cargo" "$dir/bin/dx" "$dir/bin/sqlx" "$dir/bin/curl" "$dir/bin/import" "$dir/bin/cleanup"

# A TCP port nothing uses, from 20000-29999 and outside the kernel's
# ephemeral range (ip_local_port_range): an outgoing connection takes its
# source port from that range, and one landing on a case's port would make
# its bind fail (SME-175 review 2). Never the dev server's (8080), a real
# check server's (8081) or their previews' (8181, 8182): the dx stub really
# binds the port, so a case on 8081 would collide with a persona's live
# check server. A bind test says it's free on IPv4 (and IPv6's loopback,
# when there is one); a port handed out before in this run is never handed
# out again, so two cases' ports never meet.
ports_given=$dir/ports-given
: >"$ports_given"
free_port() {
    local p
    p=$("$STUB_PYTHON3" -I - "$ports_given" <<'EOF'
import random, socket, sys
given = set(open(sys.argv[1]).read().split())
low, high = map(int, open("/proc/sys/net/ipv4/ip_local_port_range").read().split())
candidates = [p for p in range(20000, 30000)
              if not low <= p <= high and p not in (8080, 8081, 8181, 8182) and str(p) not in given]
random.shuffle(candidates)
def free(family, host, port):
    try:
        s = socket.socket(family)
    except OSError:
        return family == socket.AF_INET6  # no IPv6 at all here
    try:
        s.bind((host, port))
        return True
    except OSError as e:
        return family == socket.AF_INET6 and e.errno == 99  # no ::1 here
    finally:
        s.close()
for port in candidates[:200]:
    if free(socket.AF_INET, "0.0.0.0", port) and free(socket.AF_INET6, "::1", port):
        print(port)
        break
else:
    sys.exit(1)
EOF
) || return 1
    echo "$p" >>"$ports_given"
    echo "$p"
}
# The port check_server's servers use.
port=$(free_port) || { echo "no free port" >&2; exit 1; }

# The check worktree: a clone sharing this repo's objects, so it can check
# out any commit here without registering a worktree. Reused across runs.
worktree=$dir/worktree
[ -d "$worktree/.git" ] || git clone -q --shared --no-checkout "$PWD" "$worktree" || exit 1

export SQLX_CALLS=$dir/sqlx-calls
export DX_ENV=$dir/dx-env
# The cleanup stub's calls, and the order of the cleanup and sqlx stubs'
# lines (SME-129).
export CLEANUP_CALLS=$dir/cleanup-calls
export ORDER=$dir/order
: >"$CLEANUP_CALLS"
: >"$ORDER"
pid_file=$worktree/.check-server.pid
scratch_file=$worktree/.check-server.scratch-db
kubeconfig_file=$worktree/.check-server.kubeconfig
log_file=$worktree/.check-server.log
# Extra VAR=VALUE settings for check_server's environment, set per case.
extra_env=()
# The caller's own RUST_LOG and NO_COLOR are dropped, so they can't change
# the results.
check_server() {
    env -u SANDBOX_IMAGE -u RUST_LOG -u NO_COLOR PATH="$dir/bin:$PATH" CHECK_WORKTREE="$worktree" \
        SANDBOX_IMAGE_IMPORT="$dir/bin/import" SANDBOX_INSTANCE_CLEANUP="$dir/bin/cleanup" \
        DATABASE_URL=postgres://u@localhost/smelt_dev CHECK_PORT="$port" \
        "${extra_env[@]}" scripts/check-server "$@" 2>&1
}
scratch_url=postgres://u@localhost/smelt_scratch_worktree
# The same environment as check_server(), spelled out as an `env` command so a
# case can run it under `timeout` or `flock`, which take a command and not a
# shell function. CHECK_WORKTREE is left to the case, which usually names a
# second worktree.
cs_env=(env -u SANDBOX_IMAGE -u RUST_LOG -u NO_COLOR PATH="$dir/bin:$PATH" SANDBOX_IMAGE_IMPORT="$dir/bin/import"
    SANDBOX_INSTANCE_CLEANUP="$dir/bin/cleanup" DATABASE_URL=postgres://u@localhost/smelt_dev CHECK_PORT="$port")


# The checkout this suite runs in: finish() fails if anything in the run
# moved its HEAD. A fixture that isn't a working tree of its own lets
# check-server's `git -C "$worktree" checkout --detach` walk up and detach
# the checkout under test (SME-265); the recorded ref, not just its commit,
# so a detach that lands on the same sha still fails.
head_before=$(git rev-parse HEAD)
head_ref_before=$(git symbolic-ref -q HEAD || echo DETACHED)

# Ends a script: its failures, or that every case passed.
finish() {
    local head_now ref_now
    head_now=$(git rev-parse HEAD)
    ref_now=$(git symbolic-ref -q HEAD || echo DETACHED)
    if [ "$head_now" != "$head_before" ] || [ "$ref_now" != "$head_ref_before" ]; then
        fail "the suite moved this checkout's HEAD: '$head_ref_before $head_before' → '$ref_now $head_now'"
    fi
    if [ "$failures" -gt 0 ]; then
        echo "$failures failure(s)"
        exit 1
    fi
    echo "all $(basename "$0" | sed 's/^test-//') tests passed"
}
