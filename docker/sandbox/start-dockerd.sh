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

# SME-157 uplink mtu: begin
# discover_uplink_mtu [route-file [sysfs-net-dir]] — print the MTU of the
# interface carrying the default route, or say in a message why it couldn't
# and return non-zero. Only /proc and /sys reads in POSIX sh: the sidecar's
# image has no `ip`, and nothing here needs more than what /proc and sysfs
# already say. The two arguments exist for scripts/test-start-dockerd to run
# this against fixtures; the sidecar calls it with neither.
discover_uplink_mtu() {
    route_file=${1:-/proc/net/route}
    sys_net=${2:-/sys/class/net}
    if [ ! -r "$route_file" ]; then
        echo "start-dockerd: can't read the routing table from $route_file" >&2
        return 1
    fi
    dest=""
    iface=""
    # The first line of /proc/net/route is a header, whose Destination field
    # is the word "Destination"; a default route's is 00000000 either way
    # round.
    while read -r iface dest _; do
        if [ "$dest" = "00000000" ]; then
            break
        fi
    done < "$route_file"
    if [ "$dest" != "00000000" ]; then
        echo "start-dockerd: $route_file has no default route, so this pod has no uplink to take an MTU from" >&2
        return 1
    fi
    mtu_file=$sys_net/$iface/mtu
    if [ ! -r "$mtu_file" ]; then
        echo "start-dockerd: can't read $mtu_file: the default route is on $iface, which this container has no sysfs entry for" >&2
        return 1
    fi
    if ! mtu=$(cat "$mtu_file"); then
        echo "start-dockerd: reading $mtu_file failed" >&2
        return 1
    fi
    case $mtu in
        '' | *[!0-9]*)
            echo "start-dockerd: $mtu_file holds '$mtu' where a number was expected" >&2
            return 1
            ;;
    esac
    if [ "$mtu" -lt 1 ]; then
        echo "start-dockerd: $mtu_file holds MTU $mtu, which no interface can use" >&2
        return 1
    fi
    echo "$mtu"
}
# SME-157 uplink mtu: end

# /workspace is the conversation's own claim (SME-32), whose top directory
# belongs to whatever the storage provisioner made it; this container runs
# as root and starts first, so it hands it to the unprivileged sandbox
# user. Only the top: what's inside is already theirs.
if [ -n "${WORKSPACE_OWNER:-}" ]; then
    chown "$WORKSPACE_OWNER" /workspace
fi

# Docker's own default MTU is 1500, and a container's interface gets nothing
# else unless dockerd is told (SME-157). The pod's uplink is whatever the
# node's CNI made it — 1450 on Flannel's VXLAN, k3s's default — and a
# container advertising MSS 1460 then loses every full-MSS segment its peer
# sends: `git clone` over HTTPS dies with a recv failure, `docker build` fails
# in whichever RUN step reaches out, and it looks like a problem with the site.
# The sidecar shares the pod's network namespace, so this container's own
# default route is the pod's uplink. `--mtu` covers the default bridge;
# `--default-network-opt` is what reaches a user-defined bridge, which docker
# compose's project networks and the model's own `docker network create` are —
# measured on a real pod, `--mtu` alone leaves those at 1500 even when the
# network is created afterwards, and `--default-network-opt` alone leaves
# docker0 at 1500, so dockerd needs both.
#
# Fatal when the uplink MTU can't be read. A sidecar that won't start says so,
# in its own container log, where today's failure is silent for some hosts and
# reads as a bug in whatever was being installed.
uplink_mtu=$(discover_uplink_mtu) || {
    echo "start-dockerd: no uplink MTU, so not starting dockerd (see above)" >&2
    exit 1
}
echo "start-dockerd: every Docker bridge gets MTU $uplink_mtu from the pod's own uplink"

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

exec dockerd --cgroup-parent="$self/docker" --mtu="$uplink_mtu" \
    --default-network-opt "bridge=com.docker.network.driver.mtu=$uplink_mtu" "$@"
