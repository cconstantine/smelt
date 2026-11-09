# smelt's go watchdog (SME-456), run next to a server pod's install:
# cmd/go bounds no fetch past the TLS handshake, so a module proxy that
# stops answering holds `go install` forever. Every 5 s it checks the
# install's `go` processes (named `go`, the install's own descendants);
# after __GO_IDLE_S__ s with one running, the log not growing and under
# 1 KB/s received, it says so in the log and stops them (TERM). It ends
# with the install, or on TERM. $1: the install's pid, $2: its log,
# $3: the received-bytes table (/proc/net/dev).
install=$1 log=$2 netdev=$3 limit=__GO_IDLE_S__ nap=
trap '[ -z "$nap" ] || kill "$nap" 2>/dev/null; exit 0' TERM
size() { wc -c < "$log" 2>/dev/null || echo 0; }
received() {
  total=0
  { read -r _; read -r _; while read -r line; do set -- ${line#*:}; total=$((total + ${1:-0})); done; } < "$netdev" 2>/dev/null
  echo "$total"
}
# Whether process $1 is the install or one of its descendants.
under() {
  p=$1
  while [ -n "$p" ] && [ "$p" -gt 1 ]; do
    [ "$p" = "$install" ] && return 0
    p=$(sed -n 's/^PPid:[[:space:]]*//p' "/proc/$p/status" 2>/dev/null)
  done
  return 1
}
gos() {
  for d in /proc/[0-9]*; do
    [ "$(cat "$d/comm" 2>/dev/null)" = go ] && under "${d#/proc/}" && echo "${d#/proc/}"
  done
}
idle=0 last_size=$(size) last_received=$(received)
while kill -0 "$install" 2>/dev/null; do
  sleep 5 & nap=$!
  wait "$nap"
  nap=
  now_size=$(size) now_received=$(received) running=$(gos)
  if [ -z "$running" ] || [ "$now_size" != "$last_size" ] || [ $((now_received - last_received)) -ge 5120 ]; then
    idle=0
  else
    idle=$((idle + 5))
  fi
  last_size=$now_size last_received=$now_received
  if [ "$idle" -ge "$limit" ]; then
    echo "smelt: go wrote nothing and received under 1 KB/s for $limit s, so it was stopped" >> "$log"
    kill $running 2>/dev/null
    idle=0
  fi
done
