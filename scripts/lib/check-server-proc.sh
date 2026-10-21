# Reading /proc for check-server: who holds the port lock, who listens on a
# port, and whether the process group a pid file names is this worktree's server.
# Split out of scripts/check-server, which had reached the repo's 40,000-byte
# ceiling (SME-186 review 1, finding 4). Sourced by it and never run on its own:
# these functions read check-server's `worktree`, `port` and `port_lock` at call
# time, and signal nothing — only stop does.
# The process holding the port lock — the check server that holds it — into
# holder_pid, holder_cmd and holder_dir. The advice a waiting start gives
# names a worktree, so it has to know which one, and the program (from
# /proc/<pid>/comm, never the arguments, which can carry a secret — SME-175
# review 1). It prefers a session leader: setsid made the server's `dx` one,
# and a second start waiting on the same lock never is, so a leader is the
# server while any other process with the lock open is only waiting for it
# (SME-186 B48). With no leader — the server's died, or a start is between its
# flock and its setsid — it falls back to the oldest of the rest: whoever holds
# the file first is the older, and the processes waiting on it are younger, so
# the youngest is usually a fellow waiter, and the advice would stop it instead
# of the server (review 1, finding 1). Reads /proc only; signals nothing.
find_port_holder() {
    local own fd pid sid
    local -a leaders=() others=()
    holder_pid="" holder_cmd="" holder_dir=""
    own=$(ps -o pgid= -p $$ | tr -d ' ')
    # The scan races with processes coming and going: every child a waiting
    # start spawns while descriptor 8 is open has the lock open itself, `find`
    # first of all, and is gone again before its /proc entry is read. Hence the
    # two `|| true`s — without them errexit ends the whole start on a vanished
    # directory or pid, silently, sed's error hidden by the redirect, which is
    # what the first version of this did. A pid that has gone holds no lock.
    for fd in $(find /proc/[0-9]*/fd -maxdepth 1 -lname "$port_lock" 2>/dev/null || true); do
        pid=${fd#/proc/}
        pid=${pid%%/*}
        # This start has the lock open itself (descriptor 8), and so does any
        # start waiting beside it: neither of them is the holder.
        [ "$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')" = "$own" ] && continue
        # Field 6 of /proc/<pid>/stat is the session id. The command name is
        # field 2 and can hold spaces and `)`, so cut through the last one
        # first, as group_is_live does.
        sid=$(sed 's/.*) //' "/proc/$pid/stat" 2>/dev/null | cut -d' ' -f4 || true)
        [ -n "$sid" ] || continue
        if [ "$sid" = "$pid" ]; then
            leaders+=("$pid")
        else
            others+=("$pid")
        fi
    done
    local -a from=("${others[@]}")
    [ ${#leaders[@]} -gt 0 ] && from=("${leaders[@]}")
    [ ${#from[@]} -gt 0 ] || return 0
    holder_pid=$(printf '%s\n' "${from[@]}" | sort -n | head -n 1)
    holder_cmd=$(cat "/proc/$holder_pid/comm" 2>/dev/null || echo "an unnamed program")
    holder_dir=$(readlink "/proc/$holder_pid/cwd" 2>/dev/null || echo "an unreadable directory")
}

# The inodes of the sockets listening on TCP port $1, on any local address,
# IPv4 or IPv6 (SME-175). Read from /proc, since the container has no ss or
# lsof; a missing /proc/net/tcp6 (no IPv6) counts as none.
port_listener_inodes() { # port
    local hex file
    local -a files=()
    hex=$(printf '%04X' "$1")
    for file in /proc/net/tcp /proc/net/tcp6; do
        [ -r "$file" ] && files+=("$file")
    done
    [ ${#files[@]} -gt 0 ] || return 0
    # State 0A is LISTEN; the local address ends in :<port, 4 hex digits>.
    awk -v suffix=":$hex" '$4 == "0A" && substr($2, length($2) - 4) == suffix { print $10 }' "${files[@]}"
}

# The socket inodes the live processes of group $1 hold open.
group_socket_inodes() { # pgid
    local pid
    for pid in $(pgrep -g "$1" 2>/dev/null); do
        find "/proc/$pid/fd" -maxdepth 1 -lname 'socket:*' -printf '%l\n' 2>/dev/null || true
    done | sed -n 's/^socket:\[\([0-9]*\)\]$/\1/p'
}

# The inodes of the sockets listening on port $2 that no process of group
# $1 holds: with no group, every one of them. The listeners are read
# before the group's sockets (SME-175 review 1): read the other way round,
# a server binding between the two reads had its own new listener taken
# for a foreign one. A listener of the first read that has closed by a
# second one is left out too, since nothing listens there any more.
foreign_listener_inodes() { # pgid (may be empty) port
    local listeners ours="" inode
    listeners=$(port_listener_inodes "$2")
    # Tests only: runs between the two reads, so a case can make the
    # server bind there. Its output goes to stderr: this function's
    # stdout is the list of foreign inodes (SME-175 review 2).
    if [ -n "$1" ] && [ -n "${CHECK_SERVER_TEST_OWNERSHIP_HOOK:-}" ]; then
        "$CHECK_SERVER_TEST_OWNERSHIP_HOOK" >&2
    fi
    [ -n "$1" ] && ours=$(group_socket_inodes "$1")
    local still
    still=$(port_listener_inodes "$2")
    for inode in $listeners; do
        grep -qxF "$inode" <<<"$ours" && continue
        grep -qxF "$inode" <<<"$still" && echo "$inode"
    done
    return 0
}

# Who holds the listening sockets $@ (inodes): "pid N (program) in <cwd>"
# for each process this user can see holding one. The program's name only
# (/proc/<pid>/comm), never its arguments, which can carry a secret
# (SME-175 review 1). A socket no visible process holds is another user's
# (named by its uid) or one that has closed since it was found (review 1:
# both read as "can't see"). Only reads /proc; never signals anything.
describe_listeners() { # inode...
    local inode fd pid cmd cwd uid
    local -a found=() seen=()
    for inode in "$@"; do
        local held=""
        for fd in $(find /proc/[0-9]*/fd -maxdepth 1 -lname "socket:\[$inode\]" 2>/dev/null); do
            pid=${fd#/proc/}
            pid=${pid%%/*}
            held=1
            [[ " ${seen[*]} " == *" $pid "* ]] && continue
            seen+=("$pid")
            cmd=$(cat "/proc/$pid/comm" 2>/dev/null || echo "an unnamed program")
            cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null || echo "an unreadable directory")
            found+=("pid $pid ($cmd) in $cwd")
        done
        [ -n "$held" ] && continue
        uid=$(cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk -v i="$inode" '$4 == "0A" && $10 == i { print $8; exit }' || true)
        if [ -n "$uid" ]; then
            found+=("a process of uid $uid that this user can't see (such as docker-proxy)")
        else
            found+=("a listener that has closed since")
        fi
    done
    local IFS=";"
    echo "${found[*]}" | sed 's/;/; /g'
}

# Whether a live process is in process group $1. pgrep counts a zombie, and
# a process killed here stays a zombie until its parent notices it died: the
# parent is often gone (the server runs in its own group and session), and
# a container's PID 1 never reaps at all, so pgrep alone reports a stopped
# server as running forever. /proc/<pid>/stat names the state after the
# command name, which can hold spaces and ')'.
group_is_live() {
    local pid state
    for pid in $(pgrep -g "$1" 2>/dev/null); do
        state=$(sed 's/.*) //' "/proc/$pid/stat" 2>/dev/null | cut -d' ' -f1)
        [ -n "$state" ] || continue
        [ "$state" = Z ] || return 0
    done
    return 1
}

# The check worktree's real path, which is what /proc/<pid>/cwd says for a
# process started here: start does `cd "$worktree"` before exec'ing dx. Empty
# when there's no worktree, and then nothing can be running in it.
worktree_path() {
    (cd "$worktree" 2>/dev/null && pwd -P) || true
}

# Whether a live process in group $1 looks like this worktree's check server:
# one has a check-server port lock open — descriptor 8, which every process in
# the group inherits — and one has this worktree as its working directory. A
# group whose id has been reused meets neither, or only one: another worktree's
# server holds a lock but works elsewhere, and a stranger started in this
# worktree has the directory and no lock. This is what keeps `stop` from
# signalling a group on the pid file's word alone (SME-186 B41). Reads /proc.
group_is_our_server() { # pgid
    local pid cwd has_lock="" in_worktree=""
    cwd=$(worktree_path)
    for pid in $(pgrep -g "$1" 2>/dev/null || true); do
        if [ -z "$has_lock" ] &&
            [ -n "$(find "/proc/$pid/fd" -maxdepth 1 -lname '*/smelt-check-server-*.lock' -print -quit 2>/dev/null || true)" ]; then
            has_lock=1
        fi
        if [ -z "$in_worktree" ] && [ -n "$cwd" ] &&
            [ "$(readlink "/proc/$pid/cwd" 2>/dev/null || true)" = "$cwd" ]; then
            in_worktree=1
        fi
        [ -n "$has_lock" ] && [ -n "$in_worktree" ] && return 0
    done
    return 1
}

# The process group the pid file names, when it names one. Whether that group
# is this worktree's server is running_pgid's question; this answers only what
# the file says, so `stop` can name the group it refuses to signal.
pid_file_pgid() {
    local file
    file=$(pid_file)
    [ -s "$file" ] || return 1
    cat "$file"
}

running_pgid() {
    local file pgid
    file=$(pid_file)
    [ -s "$file" ] || return 1
    pgid=$(cat "$file")
    # Still ours if a live process in that group is.
    group_is_live "$pgid" || return 1
    # …and if that group looks like this worktree's server. A live group alone
    # isn't enough: its id can have been reused, and then it's someone else's
    # (SME-186 B41).
    group_is_our_server "$pgid" || return 1
    echo "$pgid"
}
