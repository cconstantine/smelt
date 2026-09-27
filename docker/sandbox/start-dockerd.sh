#!/bin/sh
# Starts dockerd in a sandbox pod's `docker` sidecar (SME-33), in place of
# the docker:dind image's own entrypoint. `src/sandbox.rs` compiles this in
# with `include_str!` and passes it as the sidecar's `sh -c` command; the
# arguments after it are dockerd's.
#
# A privileged container shares the node's cgroup namespace on these
# clusters, so /sys/fs/cgroup here is the node's whole hierarchy. The dind
# entrypoint assumes a private one: it moves every process in the node's
# root cgroup into /init, enables controllers at the node's root, and
# dockerd then puts containers under /docker at the node's root, outside
# the pod and every limit. Seen in SME-33's spike.
#
# This script instead keeps dockerd and every container it starts inside
# this container's own cgroup, so they count against the sidecar's memory
# limit and an OOM there kills only the sidecar. It never writes outside
# that cgroup.
set -eu

self=$(sed -n 's/^0:://p' /proc/self/cgroup)
base=/sys/fs/cgroup$self
mkdir -p "$base/dockerd"
# cgroup v2: a cgroup whose children get controllers can't hold processes
# itself, so move ours into a leaf first. Retried, because an exec probe
# can land a new process in $base between the move and the write.
until {
    for pid in $(cat "$base/cgroup.procs"); do
        echo "$pid" > "$base/dockerd/cgroup.procs" 2>/dev/null || :
    done
    sed -e 's/ / +/g' -e 's/^/+/' < "$base/cgroup.controllers" > "$base/cgroup.subtree_control"
} 2>/dev/null; do
    sleep 0.1
done

# As the dind entrypoint does: nested containers' mounts propagate like on
# a systemd host. Only this container's own mount namespace is affected.
mount --make-rshared /

exec dockerd --cgroup-parent="$self/docker" "$@"
