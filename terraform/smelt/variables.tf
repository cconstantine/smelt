# --- The images ---------------------------------------------------------------

variable "image" {
  description = <<-EOT
    The smelt server image, e.g. `ghcr.io/<you>/smelt:<sha>` — what
    `.github/workflows/publish-images.yml` pushes. Not `:latest` for anything you
    care about: this deployment updates by changing this value.
  EOT
  type        = string

  validation {
    condition     = length(var.image) > 0 && !startswith(var.image, "docker.io/library/smelt-sandbox")
    error_message = "image must be the server image, not the sandbox image."
  }
}

variable "sandbox_image" {
  description = <<-EOT
    The sandbox agent image pods are created from, e.g.
    `ghcr.io/<you>/smelt-sandbox:<sha>`. Passed as `SANDBOX_IMAGE`. Required,
    because smelt's own default names an image built into a node's containerd
    (`smelt-sandbox:src-<hash>`), which a pod in a cluster has no way to reach.
  EOT
  type        = string
}

variable "image_pull_policy" {
  description = "Passed as `SANDBOX_IMAGE_PULL_POLICY`. Only a cluster whose images are deliberately loaded onto its nodes wants `Never`."
  type        = string
  default     = "IfNotPresent"

  validation {
    condition     = contains(["Always", "IfNotPresent", "Never"], var.image_pull_policy)
    error_message = "image_pull_policy must be Always, IfNotPresent or Never."
  }
}

variable "image_pull_secret" {
  description = <<-EOT
    Name of a `kubernetes.io/dockerconfigjson` secret, created by the caller in
    `namespace`, that both server and sandbox pods pull their images with. Set it
    only for a private registry; a public GHCR package needs nothing. It is not
    created here, because the credentials are nobody's business but the caller's.
  EOT
  type        = string
  default     = null
}

# --- Where it's served --------------------------------------------------------

variable "domain" {
  description = "The host the app is reached at, e.g. `smelt.example.com`. Used for the ingress rule, `SMELT_BASE_URL` and `SMELT_ALLOWED_HOSTS`."
  type        = string
}

variable "base_url" {
  description = "Overrides `SMELT_BASE_URL` (the public address used for OAuth redirects and preview links). Defaults to `https://<domain>`."
  type        = string
  default     = null
}

variable "additional_hosts" {
  description = "Extra host names smelt may be reached at, beyond `domain` (see `SMELT_ALLOWED_HOSTS`). IPs and `localhost` always work."
  type        = list(string)
  default     = []
}

variable "preview_domain" {
  description = <<-EOT
    Wildcard domain that preview host names are built from, without the `*.`
    (see `SMELT_PREVIEW_URL`). Needs wildcard DNS, and an ingress rule with a
    host starting `*.` that reaches port 8181 (added here when set). Unset means
    no previews: the preview listener then binds `127.0.0.1:8181`, so it exists
    but is unreachable from outside the pod — the safe default for a cluster
    that hasn't done the wildcard DNS yet.
  EOT
  type        = string
  default     = null
}

variable "ingress_class_name" {
  description = "Ingress class for the rule above. Unset uses the cluster's default class."
  type        = string
  default     = null
}

variable "ingress_annotations" {
  description = "Annotations for the ingress (TLS-by-cert-manager, timeouts, whatever the cluster wants)."
  type        = map(string)
  default     = {}
}

# --- Namespaces and cluster-level limits --------------------------------------

variable "namespace" {
  description = "Where smelt and its sandboxes live. smelt creates pods, claims, secrets and services here; see the RBAC this module adds."
  type        = string
  default     = "smelt-park"
}

variable "test_namespace" {
  description = "Whether to also create smelt's own real-cluster test namespace (`smelt-park-test`) with the same RBAC. Off by default; the developer-run test suite is the only thing that uses it."
  type        = bool
  default     = false
}

variable "sandbox_pod_memory_max" {
  description = "The `LimitRange` ceiling on a sandbox pod's memory, matching k8s/smelt-park-rbac.yaml. The API server rejects a `create_pod` memory_limit above it."
  type        = string
  default     = "64Gi"
}

# --- Database ------------------------------------------------------------------

variable "database_url" {
  description = <<-EOT
    An external Postgres to use, e.g. `postgres://smelt:<pw>@db.example.com/smelt`.
    Used with `deploy_database = false`: that pair is how smelt runs against a
    database that lives elsewhere.
  EOT
  type        = string
  default     = null
  sensitive   = true
}

variable "deploy_database" {
  description = <<-EOT
    Deploy a Postgres inside the namespace, which is the default because a cluster
    reaching this point usually has none. Its connection URL is built here and kept
    in the `smelt-db` secret; nothing else need know it.

    Set false to use `database_url` instead. This is deliberately its own setting
    rather than being read off `database_url`: a connection string is sensitive,
    and a sensitive value cannot drive a resource `count` or a dynamic block's
    `for_each` at all — Terraform refuses before it ever plans.
  EOT
  type        = bool
  default     = true
}

variable "postgres_image" {
  description = "Image for the Postgres this module can deploy. Also used for the readiness check that waits for it."
  type        = string
  default     = "postgres:17-alpine"
}

variable "database_name" {
  description = "Database name inside the Postgres this module deploys."
  type        = string
  default     = "smelt"
}

variable "database_storage_size" {
  description = "Size of the claim behind the Postgres this module deploys."
  type        = string
  default     = "10Gi"
}

variable "storage_class_name" {
  description = "Storage class for every claim this module makes (Postgres, and nothing else — sandbox claims are made by smelt with the cluster default). Unset uses the cluster default."
  type        = string
  default     = null
}

# --- Sandbox pod sizing ---------------------------------------------------------

variable "sandbox_memory_limit" {
  description = "`SANDBOX_MEMORY_LIMIT`: what a sandbox pod gets when its caller doesn't ask for a size. Left unset here, so smelt's own default (8Gi) applies."
  type        = string
  default     = null
}

variable "docker_storage_size" {
  description = "`SANDBOX_DOCKER_STORAGE_SIZE`: each conversation's Docker-data claim. Unset → smelt's default (20Gi)."
  type        = string
  default     = null
}

variable "workspace_storage_size" {
  description = "`SANDBOX_WORKSPACE_STORAGE_SIZE`: each conversation's /workspace claim. Unset → smelt's default (20Gi)."
  type        = string
  default     = null
}

variable "sandbox_wait_timeout_secs" {
  description = "`SANDBOX_RUNNING_WAIT_TIMEOUT_SECS`: how long a pod start may take before smelt gives up. Slow clusters (CI on a shared runner) need it raised; see docs/setup.md. Unset → smelt's default (90)."
  type        = number
  default     = null
}

# --- The server's own sizing ----------------------------------------------------

variable "server_memory" {
  description = "Memory request and limit for the smelt server container. It is not a roomy number: webfetch and browsing sessions run headless Chrome inside this container."
  type        = string
  default     = "2Gi"
}

variable "server_cpu" {
  description = "CPU request for the smelt server container. No limit is ever set."
  type        = string
  default     = "250m"
}

variable "extra_env" {
  description = "Anything else the server needs in its environment, verbatim. Keys here must not collide with the ones this module already sets."
  type        = map(string)
  default     = {}
}
