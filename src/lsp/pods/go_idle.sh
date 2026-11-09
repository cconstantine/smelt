# smelt's go watchdog (SME-456), run next to a server pod's install:
# cmd/go bounds no fetch past the TLS handshake, so a module proxy that
# stops answering holds `go install` forever. Every 5 s it checks the
# install's `go` processes (named `go`, the install's own descendants)
# and the processes under them; after __GO_IDLE_S__ s with a go running,
# the log not growing and those processes reading under 1 KB/s, it says
# so in the log and stops the go processes (TERM). What they read is
# each one's own `rchar` (sockets and files alike), counted per process
# from one check to the next: a process seen for the first time, or one
# gone, adds nothing. Never the pod's network counters: on a busy node
# the bridge's traffic to every pod kept those growing (SME-456's
# full-gate red). It ends with the install, or on TERM.
# $1: the install's pid, $2: its log.
install=$1 log=$2 limit=__GO_IDLE_S__ gos= tree= last_reads= delta=0
# The nap's `sleep` goes too: `$!` is set the moment it starts (only the
# nap runs in the background here), so a TERM between its start and the
# `wait` doesn't leave it running.
trap 'kill $! 2>/dev/null; exit 0' TERM
size() { wc -c < "$log" 2>/dev/null || echo 0; }
# The value of field $1 in file $2 (`PPid:` in a status, `rchar:` in an
# io), in $value, empty if it can't be read. Builtins only: a check runs
# over every process, and a fork for each would be slow on a busy host.
field() {
  value=
  while read -r key rest; do
    if [ "$key" = "$1" ]; then value=$rest; break; fi
  done 2>/dev/null < "$2"
}
# The install's go processes ($gos), and each process that is one of
# them or under one ($tree).
scan() {
  gos= tree=
  for d in /proc/[0-9]*; do
    p=${d#/proc/} q=${d#/proc/} is_go= in_go=
    # Up from $p: recorded only if the walk reaches the install. The
    # install itself can be go (the shell execs its last command), so
    # each process's name is read before it's compared with the install.
    while [ -n "$q" ] && [ "$q" -gt 1 ]; do
      comm=
      read -r comm 2>/dev/null < "/proc/$q/comm"
      if [ "$comm" = go ]; then
        in_go=1
        [ "$q" != "$p" ] || is_go=1
      fi
      if [ "$q" = "$install" ]; then
        [ -z "$is_go" ] || gos="$gos $p"
        [ -z "$in_go" ] || tree="$tree $p"
        break
      fi
      field PPid: "/proc/$q/status"
      q=$value
    done
  done
}
# What $tree read since the last check ($delta), per process.
reads() {
  delta=0 now_reads=
  for p in $tree; do
    field rchar: "/proc/$p/io"
    r=$value
    [ -n "$r" ] || continue
    now_reads="$now_reads $p:$r"
    for e in $last_reads; do
      if [ "${e%%:*}" = "$p" ] && [ "$r" -gt "${e#*:}" ]; then delta=$((delta + r - ${e#*:})); fi
    done
  done
  last_reads=$now_reads
}
idle=0 last_size=$(size)
scan
reads
while kill -0 "$install" 2>/dev/null; do
  sleep 5 &
  wait $!
  now_size=$(size)
  scan
  reads
  if [ -z "$gos" ] || [ "$now_size" != "$last_size" ] || [ "$delta" -ge 5120 ]; then
    idle=0
  else
    idle=$((idle + 5))
  fi
  last_size=$now_size
  if [ "$idle" -ge "$limit" ]; then
    echo "smelt: go wrote nothing and received under 1 KB/s for $limit s, so it was stopped" >> "$log"
    kill $gos 2>/dev/null
    idle=0
  fi
done
