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
