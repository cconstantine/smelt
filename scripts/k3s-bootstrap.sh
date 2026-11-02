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
# Only a speed tweak, so nothing in it may hold up or fail the bootstrap
# (the kubeconfig above is what `smelt` waits for): a bounded wait for the
# deployment, and a warning, never an exit, when a step fails (SME-341
# review). Each step checks its own status, since `set -e` doesn't apply
# inside a function called with `||`.
tune_provisioner() {
    echo "waiting for the local-path provisioner..."
    waited=0
    until kctl -n kube-system get deployment local-path-provisioner >/dev/null 2>&1; do
        waited=$((waited + 2))
        [ "$waited" -lt 120 ] || { echo "no local-path provisioner after 120 s"; return 1; }
        sleep 2
    done
    current=$(kctl -n kube-system get deployment local-path-provisioner -o jsonpath='{.spec.template.spec.containers[0].args}') || return 1
    [ "$current" = "$PROVISIONER_ARGS" ] && return 0
    kctl -n kube-system patch deployment local-path-provisioner --type=json \
        -p "[{\"op\":\"add\",\"path\":\"/spec/template/spec/containers/0/args\",\"value\":$PROVISIONER_ARGS}]" || return 1
    if kctl -n kube-system rollout status deployment local-path-provisioner --timeout=90s; then
        echo "local-path provisioner: $PROVISIONER_ARGS"
        return 0
    fi
    echo "the local-path provisioner didn't come up with $PROVISIONER_ARGS; rolling it back"
    kctl -n kube-system rollout undo deployment local-path-provisioner || return 1
    kctl -n kube-system rollout status deployment local-path-provisioner --timeout=90s || return 1
    return 1
}
tune_provisioner || echo "warning: the local-path provisioner is left as k3s set it up; the pod tests' claims bind slower" >&2
