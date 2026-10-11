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
  `k8s/smelt-park-rbac.yaml`, and a LimitRange capping sandbox pod memory;
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
- OAuth and anything else secret go in through `extra_env`; the module has no
  opinion about them.
- The pod gets its cluster access from its own ServiceAccount token — no
  kubeconfig is mounted, and none should be.

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
header comment for the exact recipe.
