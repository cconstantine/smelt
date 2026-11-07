# The smelt Terraform module

Deploys a whole smelt install to a Kubernetes cluster: the server, its RBAC,
optionally its Postgres, and the ingress it's served at. Sandbox pods are not
made by Terraform — smelt itself makes them, from `sandbox_image`, in the
namespace this module sets up.

The module lives here, in the smelt repo, because it ships with the thing it
deploys: the environment variable names, the RBAC, and the sandbox pod spec
change together, and a change to one is usually a change to the others. The
consumer (e.g. the homelab repo) pulls it as a git source.

## What it makes

In `namespace` (default `smelt-park`):

- the namespace itself, a `park` ServiceAccount, a Role/RoleBinding mirroring
  `k8s/smelt-park-rbac.yaml`, and a LimitRange capping sandbox pod memory and
  declaring `default`/`defaultRequest` explicitly (SME-335);
- one `smelt` server Deployment (Recreate, not RollingUpdate — a new server
  applies its migrations at startup, and two writers on one schema is how
  that goes wrong), Service, and Ingress;
- unless told otherwise: a single-replica Postgres StatefulSet with a generated
  password in the `smelt-db` secret.

`test_namespace = true` adds `smelt-park-test` with the same RBAC, which is
where smelt's own real-cluster test suite runs.

## Things a caller must know

- `sandbox_image` is required. smelt's compiled default names an image built
  into a dev node's containerd; pods in a cluster can't reach it.
- Images come from GHCR tags published by `.github/workflows/publish-images.yml`.
  Deploy by pinning tags, never `:latest`.
- The database is a choice, not a guess: leave `deploy_database` true to get a
  Postgres here, or set it false *and* pass `database_url`. Terraform can't
  read its own decision out of a sensitive value, hence the explicit switch.
- Previews are off unless `preview_domain` is set (wildcard DNS plus a rule
  reaching port 8181 is the prerequisite; without it the preview listener
  binds loopback inside the pod, which is the safe default).
- Plain settings go in through `extra_env`, verbatim into the pod spec.
  **Secrets go in through `secret_env`** (a sensitive `map(string)`, default
  `{}`), e.g. `secret_env = { CLAUDE_CODE_OAUTH_TOKEN = var.claude_code_oauth_token }`
  with that variable `sensitive = true` and exported as `TF_VAR_…` at plan and
  apply time, so the value never reaches git:
  - The module puts the map in a `smelt-secret-env` secret in `namespace`
    (made only when the map isn't empty), and gives the server one env var per
    key, of the key's name, from a `secretKeyRef` on it. The Deployment holds
    the names, never the values, and the plan shows them as sensitive.
  - The keys are no secret: they show in the plan and the Deployment.
  - The plan refuses an empty value (a `TF_VAR_x="$(cat <missing file>)"`
    export sets one, and a variable with no default doesn't catch it), a key
    that isn't an environment variable name, or one that
    `extra_env` or this module sets, `DATABASE_URL` included. Every name the
    module can set counts, set or not, so turning a setting on later can't
    clash.
  - **A changed value rolls the server pod.** The pod template carries
    `smelt/secret-env-sha256`, a SHA-256 of the whole map, which gives no value
    away. A deployment with no `secret_env` gets no secret and no annotation, so
    upgrading the module doesn't roll it.
- **amd64 or arm64 nodes, or a mix.** Images CI publishes from SME-360
  (#248, 6d2831d) on are built for linux/amd64 and linux/arm64, and so are
  `postgres_image`'s default (`postgres:17-alpine`) and smelt's default Docker
  sidecar (`docker:29-dind`, which `SANDBOX_DOCKER_IMAGE` in `extra_env`
  changes). So no pod template here pins an architecture, and neither does
  smelt's own sandbox pod spec.
  - **Tags from before #248 are amd64 only.** On a mixed cluster, move `image`
    and `sandbox_image` to a later tag in the same apply that picks up this
    module, or set `node_selector = { "kubernetes.io/arch" = "amd64" }` until
    you do.
  - **`node_selector` reaches only the server and the database.** Sandbox
    pods take `SANDBOX_NODE_SELECTOR` through `extra_env`, e.g.
    `{ SANDBOX_NODE_SELECTOR = "kubernetes.io/arch=amd64" }`. Set it when
    some nodes can't run them: a single-arch `sandbox_image`, or a kernel
    that won't start the Docker sidecar (an arm64 Jetson's doesn't).
    Language-server pods follow their sandbox pod's node (SME-379).
  - **Applying a change to `node_selector` rolls both pods at once:** the
    database's one pod and the server (Recreate). The server's init container
    waits about 2 minutes for Postgres, so it's a short outage. The database
    pod returns to its node either way, since its `local-path` claim is
    node-local.
- The pod gets its cluster access from its own ServiceAccount token — no
  kubeconfig is mounted, and none should be.

## Terraform version

Terraform 1.7 or later (`versions.tf`). `secret_env`'s validations call
`nonsensitive()`, which 1.5 and 1.6 refuse inside a validation even for the
empty default, so on those every plan would fail.

## Kubernetes version

Sandbox pods use native sidecar containers (init containers with
`startupProbe`/`restartPolicy: Always`). That needs a cluster no older than
Kubernetes 1.29; the cluster this was verified on runs k3s v1.34.6, matching
the pin in `docs/setup.md` and `src/bin/sandbox_image_import.rs`. On an older
cluster every `create_pod` fails with a 422 about `startupProbe` on an init
container.

## Testing

`terraform/test-cluster/` applies this module to a k3d cluster, and smelt's
real-cluster pod-lifecycle tests run against the result. See that directory's
header comment for the exact recipe. Its `secret_env` carries a dummy value,
and `terraform/test-cluster/check-secret-env.sh` checks the result: the server
reads it from a `secretKeyRef`, `kubectl get deploy -o yaml` holds no plain
copy, the checksum annotation is there, and the running server sees the value.
Applying another value (`-var secret_env_value=…`) changes the checksum and
rolls the pod. `terraform/test-cluster/check-limitrange.sh` checks the
LimitRange likewise: the plan converges, `defaultRequest` reads back the
declared `0`, a pod naming no resources runs at request 0, a sandbox-like one
is untouched, and a hand-applied `k8s/smelt-park-rbac.yaml` stays idempotent.
