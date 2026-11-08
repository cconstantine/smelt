#!/usr/bin/env bash
# Checks what SME-335 promises, against the test cluster main.tf applied: the
# terraform plan converges (exit 0), the live `sandbox-pod-max` LimitRanges
# read back the declared `defaultRequest: "0"` (an omitted one the API server
# fills from `max` on every write — that fill-in is the bug: the plan never
# closes, and every container naming no memory of its own reserved the
# ceiling), a pod naming no resources runs at all and reserves nothing, and a
# pod naming its own is untouched by the defaults. Then it applies
# `k8s/smelt-park-rbac.yaml` twice by hand and re-reads the live objects: the
# dev-cluster route must be idempotent with the same explicit shape.
#
#   terraform apply && ./check-limitrange.sh
#
# Run from this directory (the plan runs there, and the yaml is a relative
# path). KUBECTL names the client (`docker exec -i <k3s container> kubectl`
# works for a cluster in a container); TERRAFORM names terraform if that isn't
# on PATH; NAMESPACE is the module's namespace, and its `-test` sibling — the
# one `test_namespace = true` (main.tf) adds — is checked too. The probe pods
# reserve nothing and the script deletes them on exit.
set -euo pipefail

cd "$(dirname "$0")"

read -r -a kubectl <<<"${KUBECTL:-kubectl}"
read -r -a terraform <<<"${TERRAFORM:-terraform}"
ns=${NAMESPACE:-smelt-park}
probe_image=${PROBE_IMAGE:-postgres:17-alpine}
namespaces=("$ns" "$ns-test")
failed=0
probes=(sme335-no-resources sme335-sandbox-like)

fail() {
    echo "FAIL: $*" >&2
    failed=1
}

cleanup() {
    for p in "${probes[@]}"; do
        "${kubectl[@]}" -n "$ns" delete pod "$p" --ignore-not-found --wait=false \
            >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT

# 1. The plan closes. Before SME-335 this exits 2, re-proposing
#    `default`/`default_request 64Gi -> null` forever: a config omitting what
#    the API server fills can never converge.
plan_rc=0
plan_out=$("${terraform[@]}" plan -detailed-exitcode 2>&1) || plan_rc=$?
if [ "$plan_rc" -ne 0 ]; then
    fail "terraform plan -detailed-exitcode exited $plan_rc, not 0; its output:"
    echo "$plan_out"
fi

# 2. Both namespaces' live objects carry the declared default request — the
#    fill-in, and any hand drift, reads a max-sized request back here.
for n in "${namespaces[@]}"; do
    got=$("${kubectl[@]}" -n "$n" get limitrange sandbox-pod-max \
        -o jsonpath='{.spec.limits[0].defaultRequest.memory}' 2>/dev/null || echo missing)
    [ "$got" = 0 ] || fail "namespace $n: sandbox-pod-max has defaultRequest.memory '$got', not 0"
done

# 3. A container naming no resources schedules at all, reserving nothing.
#    The unfixed fill-in made its request the whole ceiling, which no node
#    under it could schedule. The request value is the node-independent
#    half; this half is the bug's own symptom.
"${kubectl[@]}" -n "$ns" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${probes[0]}
spec:
  containers:
    - name: probe
      image: $probe_image
      command: ["sh", "-c", "sleep 3600"]
EOF
phase=Unknown
for _ in $(seq 45); do
    phase=$("${kubectl[@]}" -n "$ns" get pod "${probes[0]}" -o jsonpath='{.status.phase}' 2>/dev/null || echo Unknown)
    [ "$phase" = Running ] && break
    sleep 2
done
req=$("${kubectl[@]}" -n "$ns" get pod "${probes[0]}" -o jsonpath='{.spec.containers[0].resources.requests.memory}')
lim=$("${kubectl[@]}" -n "$ns" get pod "${probes[0]}" -o jsonpath='{.spec.containers[0].resources.limits.memory}')
[ "$req" = 0 ] || fail "no-resources probe got requests.memory '$req' (limit '$lim'), not 0"
[ "$phase" = Running ] || fail "no-resources probe never reached Running (phase '$phase', requests.memory '$req')"

# 4. A sandbox-like container — explicit zero request, 8Gi limit, the shape
#    `build_pod_spec` builds (SME-77) — is untouched by either default.
"${kubectl[@]}" -n "$ns" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${probes[1]}
spec:
  containers:
    - name: probe
      image: $probe_image
      command: ["sh", "-c", "sleep 3600"]
      resources:
        requests:
          memory: "0"
        limits:
          memory: 8Gi
EOF
req=$("${kubectl[@]}" -n "$ns" get pod "${probes[1]}" -o jsonpath='{.spec.containers[0].resources.requests.memory}')
lim=$("${kubectl[@]}" -n "$ns" get pod "${probes[1]}" -o jsonpath='{.spec.containers[0].resources.limits.memory}')
[ "$req" = 0 ] || fail "sandbox-like probe's requests.memory became '$req': a defaultRequest must not touch a container naming its own"
[ "$lim" = 8Gi ] || fail "sandbox-like probe's limits.memory became '$lim', not 8Gi"

# 5. The dev-cluster route: kubectl apply replaces the whole atomic `limits`
#    list, so a hand-applied yaml must land the same explicit shape and stay
#    there. Twice, because "idempotent" is the property the dev k3s re-boostrap
#    leans on. The yaml goes in through stdin so a KUBECTL that runs kubectl
#    in another container (main.tf's note) still reads this repo's file.
rbac_yaml=../../k8s/smelt-park-rbac.yaml
for _ in 1 2; do
    "${kubectl[@]}" apply -f - >/dev/null <"$rbac_yaml"
done
for n in "${namespaces[@]}"; do
    got=$("${kubectl[@]}" -n "$n" get limitrange sandbox-pod-max \
        -o jsonpath='{.spec.limits[0].defaultRequest.memory}' 2>/dev/null || echo missing)
    [ "$got" = 0 ] || fail "namespace $n: after two kubectl applies of $rbac_yaml, defaultRequest.memory is '$got', not 0"
done

[ "$failed" -eq 0 ] && echo "limitrange: plan converges, defaultRequest 0 in ${namespaces[*]}, no-resources pod Running at request 0, sandbox-like pod unchanged, $rbac_yaml idempotent"
exit "$failed"
