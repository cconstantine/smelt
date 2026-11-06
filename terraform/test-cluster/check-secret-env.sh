#!/usr/bin/env bash
# Checks what `secret_env` promises, against the test cluster main.tf applied:
# the server container takes SMELT_TEST_SECRET_ENV from a secretKeyRef on the
# `smelt-secret-env` secret, the Deployment (as `kubectl get deploy -o yaml`
# prints it) holds no plain copy of the value, its pod template carries the
# `smelt/secret-env-sha256` checksum (which isn't the value either), and the
# running server sees the value in its environment.
#
#   terraform apply && ./check-secret-env.sh
#
# Then the roll: note the checksum and the pod it prints, apply again with
# `-var secret_env_value=<another>`, run this with SECRET_ENV_VALUE=<another>,
# and both have changed.
#
# KUBECTL names the client (`docker exec -i <k3s container> kubectl` works for
# a cluster in a container); SECRET_ENV_VALUE is the value applied, main.tf's
# default unless you passed another. The value is a dummy, not a credential.
set -euo pipefail

read -r -a kubectl <<<"${KUBECTL:-kubectl}"
value=${SECRET_ENV_VALUE:-k3d-test-not-a-secret}
ns=${NAMESPACE:-smelt-park}
failed=0

fail() {
    echo "FAIL: $*" >&2
    failed=1
}

k() {
    "${kubectl[@]}" -n "$ns" "$@"
}

env_path='{.spec.template.spec.containers[?(@.name=="smelt")].env[?(@.name=="SMELT_TEST_SECRET_ENV")]'

ref_name=$(k get deploy smelt -o jsonpath="${env_path}.valueFrom.secretKeyRef.name}")
ref_key=$(k get deploy smelt -o jsonpath="${env_path}.valueFrom.secretKeyRef.key}")
plain=$(k get deploy smelt -o jsonpath="${env_path}.value}")

[ "$ref_name" = smelt-secret-env ] || fail "SMELT_TEST_SECRET_ENV's secretKeyRef names '$ref_name', not smelt-secret-env"
[ "$ref_key" = SMELT_TEST_SECRET_ENV ] || fail "SMELT_TEST_SECRET_ENV's secretKeyRef key is '$ref_key'"
[ -z "$plain" ] || fail "SMELT_TEST_SECRET_ENV has a plain value in the Deployment"

if k get deploy smelt -o yaml | grep -qF -- "$value"; then
    fail "kubectl get deploy -o yaml holds the value in plain text"
fi

checksum=$(k get deploy smelt -o jsonpath='{.spec.template.metadata.annotations.smelt/secret-env-sha256}')
if ! [[ "$checksum" =~ ^[0-9a-f]{64}$ ]]; then
    fail "the pod template's smelt/secret-env-sha256 annotation is '$checksum', not a SHA-256"
fi

pods=$(k get pods -l app.kubernetes.io/name=smelt,'!app.kubernetes.io/component' \
    --field-selector=status.phase=Running -o jsonpath='{.items[*].metadata.name}')
pod=${pods%% *}
if [ -z "$pod" ]; then
    fail "no running server pod"
elif [ "$(k exec "$pod" -c smelt -- printenv SMELT_TEST_SECRET_ENV)" != "$value" ]; then
    # Compared, never printed.
    fail "the server pod $pod doesn't see the applied value as SMELT_TEST_SECRET_ENV"
fi

echo "checksum $checksum"
[ -z "$pod" ] || echo "pod $pod $(k get pod "$pod" -o jsonpath='{.metadata.uid}')"
[ "$failed" -eq 0 ] && echo "secret_env: ok"
exit "$failed"
