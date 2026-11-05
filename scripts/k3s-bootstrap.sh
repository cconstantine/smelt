#!/bin/sh
# Run once per `docker compose up` by the `k3s-bootstrap` service (see
# docker-compose.yml). Applies k8s/smelt-park-rbac.yaml to the compose-
# provided k3s cluster, mints a long-lived token for the `park` service
# account, and writes a ready-to-use kubeconfig for it — `smelt`'s own
# KUBECONFIG points directly at the file this script produces, no
# runtime rewriting needed.
#
# Idempotent: safe to run against a cluster that already has the
# namespace/SA/Role/token from a previous `docker compose up` (state
# persists in the named k3s volume).
set -eu

ADMIN_KUBECONFIG=/k3s-admin/k3s.yaml
OUT_KUBECONFIG=/out/park-kubeconfig.yaml
SERVER=https://k3s:6443

kctl() {
    kubectl --kubeconfig="$ADMIN_KUBECONFIG" --server="$SERVER" "$@"
}

echo "waiting for k3s API..."
until kctl get --raw=/healthz >/dev/null 2>&1; do
    sleep 2
done

kctl apply -f /k8s/smelt-park-rbac.yaml

kctl apply -f - <<'EOF'
apiVersion: v1
kind: Secret
metadata:
  name: park-token
  namespace: smelt-park
  annotations:
    kubernetes.io/service-account.name: park
type: kubernetes.io/service-account-token
EOF

echo "waiting for park-token to populate..."
until kctl get secret park-token -n smelt-park -o jsonpath='{.data.token}' 2>/dev/null | grep -q .; do
    sleep 1
done

TOKEN=$(kctl get secret park-token -n smelt-park -o jsonpath='{.data.token}' | base64 -d)
CA=$(kctl get secret park-token -n smelt-park -o jsonpath='{.data.ca\.crt}')

cat > "$OUT_KUBECONFIG" <<EOF
apiVersion: v1
kind: Config
clusters:
- cluster:
    server: $SERVER
    certificate-authority-data: $CA
  name: k3s
contexts:
- context:
    cluster: k3s
    namespace: smelt-park
    user: park
  name: smelt-park
current-context: smelt-park
users:
- name: park
  user:
    token: $TOKEN
EOF

echo "wrote $OUT_KUBECONFIG"

# The pod lifecycle tests' volume claims bind through k3s's bundled
# local-path provisioner, which creates and deletes a helper pod for every
# claim. The helper finishes within a second, but the provisioner's own
# Kubernetes client is held to client-go's default 5 requests a second
# (burst 10), and with several tests starting pods its requests waited
# 1-6 s each ("client-side throttling" in its log): a claim that binds in
# 2-4 s alone took 15-17 s, and twice that with eight tests at once
# (SME-341's speed pass). Raised here, after k3s has deployed it, with more
# workers for the queue. A provisioner that won't start with these flags is
# rolled back, with a warning, rather than leaving the cluster without
# storage. Patched on each `compose up`, since k3s may put its own manifest
# back on a restart.
PROVISIONER_ARGS='["--worker-threads=16","--kube-client-qps=50","--kube-client-burst=100"]'
# Only a speed tweak, so it never fails the bootstrap that `smelt` waits
# for, and its waits are bounded: on a broken cluster the deployment wait
# (120 s), the rollout (90 s) and an undo's rollout (90 s) can still put
# `smelt`'s start off by about 5 minutes, then warn (SME-341 review). Each
# step checks its own status, since `set -e` doesn't apply inside a
# function called with `||`. Returns 1 when the provisioner is left as k3s
# set it up, 2 when an undo failed and its state isn't known.
tune_provisioner() {
    # Every API call bounded too, so the step's worst case holds.
    tkctl() { kctl --request-timeout=15s "$@"; }
    echo "waiting for the local-path provisioner..."
    waited=0
    until tkctl -n kube-system get deployment local-path-provisioner >/dev/null 2>&1; do
        waited=$((waited + 2))
        [ "$waited" -lt 120 ] || { echo "no local-path provisioner after 120 s"; return 1; }
        sleep 2
    done
    current=$(tkctl -n kube-system get deployment local-path-provisioner -o jsonpath='{.spec.template.spec.containers[0].args}') || return 1
    [ "$current" = "$PROVISIONER_ARGS" ] && return 0
    tkctl -n kube-system patch deployment local-path-provisioner --type=json \
        -p "[{\"op\":\"add\",\"path\":\"/spec/template/spec/containers/0/args\",\"value\":$PROVISIONER_ARGS}]" || return 1
    if tkctl -n kube-system rollout status deployment local-path-provisioner --timeout=90s; then
        echo "local-path provisioner: $PROVISIONER_ARGS"
        return 0
    fi
    echo "the local-path provisioner didn't come up with $PROVISIONER_ARGS; rolling it back"
    tkctl -n kube-system rollout undo deployment local-path-provisioner || return 2
    tkctl -n kube-system rollout status deployment local-path-provisioner --timeout=90s || return 2
    return 1
}
tune_provisioner || case $? in
    2) echo "warning: rolling the local-path provisioner back failed; check it (kubectl -n kube-system get deployment local-path-provisioner)" >&2 ;;
    *) echo "warning: the local-path provisioner is left as k3s set it up; the pod tests' claims bind slower" >&2 ;;
esac

# Pre-pull the images test pods start from (SME-391). The LSP catalog tests
# create server pods from the catalog's own images (rust:1, node:22-slim,
# golang:1, debian:trixie-slim), and every claim the tests bind is served by
# the local-path provisioner's helper pod — scripts/cluster-prepull-images.txt
# lists those plus the helper's own busybox; on a
# fresh cluster those pulled live from Docker Hub mid-suite, and one flaked
# pull (`tls: bad record MAC`) cost a full gate. Pulling them here, before
# any test runs, moves that download to where a failure is loud and a retry
# cheap; once the node holds an image, the server pods'
# `imagePullPolicy: IfNotPresent` means no test touches the network for it —
# the proof is in SME-391's trail: with Docker Hub blocked at the mirror
# after bootstrap, the catalog test goes green; on the unfixed tree, red.
#
# The pull is the kubelet's own, through one small pod per image that just
# starts and exits: same store, same resolution and pull policy the real
# pods will use, no registry client to add to this image and no containerd
# socket to mount. On a warm cluster the pod's image is already present and
# it exits unseen — this stays cheap enough to run on every `compose up`.
#
# Unlike the provisioner tune this DOES fail the bootstrap: a node missing
# these images is the suite's exact SME-391 failure, minutes deeper in.
# Fail loudly, name the image, say what to do; `SMELT_SKIP_PREPULL=1`
# opts out for a machine that can't reach Docker Hub right now (the LSP
# catalog tests will then behave as they always did).
PREPULL_WAIT=${SMELT_PREPULL_WAIT:-1800}
prepull_images() {
    # `default` namespace: none of smelt's listings look there. Small
    # explicit requests and limits throughout (SME-314), kept under what a
    # namespace LimitRange could fill in.
    pkctl() { kctl --request-timeout=15s -n default "$@"; }
    n=0
    while read -r ref _; do
        case "$ref" in ''|'#'*) continue ;; esac
        n=$((n + 1))
        name="smelt-prepull-$n"
        echo "pre-pulling $ref onto the test node..."
        pkctl delete pod "$name" --ignore-not-found --grace-period=0 --wait >/dev/null 2>&1
        pkctl apply -f - <<EOF || return 1
apiVersion: v1
kind: Pod
metadata:
  name: $name
  labels:
    app: smelt-prepull
spec:
  restartPolicy: Never
  activeDeadlineSeconds: $PREPULL_WAIT
  automountServiceAccountToken: false
  containers:
  - name: prepull
    image: $ref
    imagePullPolicy: IfNotPresent
    command: ["sh", "-c", "echo pre-pulled"]
    resources:
      requests: {cpu: 100m, memory: 64Mi}
      limits: {cpu: 500m, memory: 256Mi}
EOF
        waited=0
        while :; do
            phase=$(pkctl get pod "$name" -o jsonpath='{.status.phase}' 2>/dev/null || echo "")
            [ "$phase" = "Succeeded" ] && break
            if [ "$phase" = "Failed" ]; then
                pkctl describe "$name" | sed -e '/^Events:/,$p' >&2
                echo "the pre-pull pod for $ref Failed (its events above; usually Docker Hub is unreachable from this cluster)" >&2
                return 1
            fi
            # The pod caps itself at activeDeadlineSeconds; this wait caps a
            # pod that never gets that far, and says which image it was for.
            waited=$((waited + 5))
            if [ "$waited" -ge $((PREPULL_WAIT + 60)) ]; then
                pkctl describe "$name" | sed -e '/^Events:/,$p' >&2
                echo "pre-pulling $ref didn't finish within ${PREPULL_WAIT}s (Docker Hub slow or unreachable from this cluster?)" >&2
                return 1
            fi
            sleep 5
        done
        pkctl delete pod "$name" --grace-period=0 >/dev/null 2>&1
    done < /scripts/cluster-prepull-images.txt
    echo "pre-pulled $n image(s) onto the test node"
}
if [ "${SMELT_SKIP_PREPULL:-}" = 1 ]; then
    echo "SMELT_SKIP_PREPULL=1: skipping the test images' pre-pull"
elif ! prepull_images; then
    echo "the test cluster couldn't pre-pull its images (above): check that Docker Hub is reachable from it," >&2
    echo "then re-run: docker compose up --exit-code-from k3s-bootstrap k3s-bootstrap" >&2
    exit 1
fi
