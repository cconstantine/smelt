#!/bin/sh
# Deletes every pod and every claim in the k3s cluster's
# `smelt-park-test` namespace — the
# namespace only `cargo test` uses (src/sandbox.rs picks it under
# #[cfg(test)]). Test runs that fail part-way, and the browser tier's
# sandbox-panel scenario, can leave sandbox pods behind; enough of them and
# new pods stop starting. The namespace is fixed here on purpose: this never
# touches `smelt-park`, where a dev instance's sandboxes live.
#
# It also deletes a running test's pods, so it takes the cluster lock
# (scripts/with-cluster-lock), waiting for a test run to finish (SME-100).
#
# Uses the service-account kubeconfig at $KUBECONFIG (kubectl isn't
# installed in the dev container). Run from inside the `smelt` container:
#   scripts/clean-test-namespace.sh
set -eu

exec "$(dirname "$0")/with-cluster-lock" python3 - "${KUBECONFIG:?KUBECONFIG must point at the cluster's kubeconfig}" <<'EOF'
import base64, json, re, ssl, sys, tempfile, urllib.request

NAMESPACE = "smelt-park-test"

config = open(sys.argv[1]).read()
field = lambda name: re.search(rf"{name}:\s*(\S+)", config).group(1)
server, token = field("server"), field("token")
ca = tempfile.NamedTemporaryFile(delete=False)
ca.write(base64.b64decode(field("certificate-authority-data")))
ca.close()
tls = ssl.create_default_context(cafile=ca.name)


def call(method, path, body=None):
    request = urllib.request.Request(
        server + path,
        method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
    )
    return json.load(urllib.request.urlopen(request, context=tls))


pods = f"/api/v1/namespaces/{NAMESPACE}/pods"
names = [pod["metadata"]["name"] for pod in call("GET", pods)["items"]]
for name in names:
    call("DELETE", f"{pods}/{name}", {"gracePeriodSeconds": 0})
print(f"deleted {len(names)} pod(s) from {NAMESPACE}" + (": " + ", ".join(names) if names else ""))

# Every claim in the namespace. Kubernetes finishes deleting one once no
# pod mounts it. (SME-255: this used to name only the Docker data ones,
# and a killed run's workspace-, volume- and agent-volume claims survived
# it.)
claims = f"/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims"
names = [claim["metadata"]["name"] for claim in call("GET", claims)["items"]]
for name in names:
    call("DELETE", f"{claims}/{name}")
print(f"deleted {len(names)} claim(s) from {NAMESPACE}" + (": " + ", ".join(names) if names else ""))
EOF
