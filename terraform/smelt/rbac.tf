# What a sandbox needs from the cluster, and what smelt needs to be able to
# ask for it. This is k8s/smelt-park-rbac.yaml expressed in Terraform: the same
# `park` ServiceAccount, the same two Role/RoleBinding pairs, the same LimitRange
# ceiling on a sandbox pod's memory, and — only with `storage_reader_enabled` —
# k8s/smelt-storage-reader.yaml's read-only storage ClusterRole and binding. Keeping it here means a deployment gets the
# RBAC with the app rather than by applying a second file by hand — and if the
# yaml ever changes, this must change with it.

locals {
  # smelt's own real-cluster tests run in a namespace of their own so they can
  # never collide with a running instance's pods. Only that needs it.
  test_namespace_name = "smelt-park-test"

  sandbox_namespaces = var.test_namespace ? toset([var.namespace, local.test_namespace_name]) : toset([var.namespace])

  # The `park` Role, transcribed from k8s/smelt-park-rbac.yaml.
  park_role_rules = [
    {
      api_groups = [""]
      resources  = ["pods"]
      verbs      = ["list", "watch", "create", "patch", "get", "delete"]
    },
    {
      api_groups = [""]
      resources  = ["pods/exec"]
      verbs      = ["create", "get"]
    },
    {
      api_groups = [""]
      resources  = ["pods/portforward"]
      verbs      = ["create", "get"]
    },
    {
      api_groups = [""]
      resources  = ["pods/log"]
      verbs      = ["get"]
    },
    {
      api_groups = [""]
      resources  = ["persistentvolumeclaims"]
      verbs      = ["list", "watch", "create", "patch", "get", "delete"]
    },
    {
      api_groups = [""]
      resources  = ["secrets", "configmaps", "services"]
      verbs      = ["list", "create", "patch", "get", "delete"]
    },
    {
      api_groups = ["networking.k8s.io"]
      resources  = ["ingresses"]
      verbs      = ["list", "create", "patch", "get", "delete"]
    },
    {
      api_groups = [""]
      resources  = ["endpoints", "events", "limitranges", "resourcequotas"]
      verbs      = ["list", "get"]
    },
    # Live memory/CPU use for the pods view, from metrics-server.
    {
      api_groups = ["metrics.k8s.io"]
      resources  = ["pods"]
      verbs      = ["get", "list"]
    },
    {
      api_groups = ["discovery.k8s.io"]
      resources  = ["endpointslices"]
      verbs      = ["list", "get"]
    }
  ]
}

resource "kubernetes_namespace_v1" "park" {
  metadata {
    name = var.namespace

    labels = {
      "app.kubernetes.io/name" = "smelt"
    }
  }
}

resource "kubernetes_namespace_v1" "park_test" {
  for_each = var.test_namespace ? toset([local.test_namespace_name]) : toset([])

  metadata {
    name = each.value

    labels = {
      "app.kubernetes.io/name" = "smelt"
    }
  }
}

resource "kubernetes_service_account_v1" "park" {
  metadata {
    name      = "park"
    namespace = kubernetes_namespace_v1.park.metadata[0].name
  }
}

# One Role per namespace, all from the same rules. The test namespace's is a
# copy, not a shared object — Roles are namespaced.
resource "kubernetes_role_v1" "park" {
  for_each = local.sandbox_namespaces

  metadata {
    name      = "park"
    namespace = each.value
  }

  dynamic "rule" {
    for_each = local.park_role_rules

    content {
      api_groups = rule.value.api_groups
      resources  = rule.value.resources
      verbs      = rule.value.verbs
    }
  }
}

resource "kubernetes_role_binding_v1" "park" {
  for_each = local.sandbox_namespaces

  metadata {
    name      = "park"
    namespace = each.value
  }

  subject {
    kind      = "ServiceAccount"
    name      = kubernetes_service_account_v1.park.metadata[0].name
    namespace = kubernetes_namespace_v1.park.metadata[0].name
  }

  role_ref {
    api_group = "rbac.authorization.k8s.io"
    kind      = "Role"
    name      = "park"
  }
}

# Read-only and cluster-wide, because PersistentVolumes and StorageClasses have
# no namespace (SME-521): smelt reads which class its claims get, with its
# reclaim policy, and counts this namespace's Released/Failed volumes. Without
# it both reads come back 403 and smelt carries on without them.
#
# Off unless `storage_reader_enabled`: these are the module's only
# cluster-wide RBAC objects (its namespaces are cluster-scoped as well, but
# any deployer of this module already makes those), and a deployer bound to
# its own namespaces can't create them (it needs `create` on clusterroles/clusterrolebindings, and
# `escalate`/`bind` on this role, since it doesn't hold these reads itself).
# Turn it on only once the deployer can, or have an admin apply
# k8s/smelt-storage-reader.yaml instead.
#
# Named `smelt-storage-reader` for the default namespace, matching that yaml,
# and `smelt-storage-reader-<namespace>` otherwise: the suffix can't make the
# default's name, so two deployments in one cluster never share one binding.
locals {
  storage_reader_name = var.namespace == "smelt-park" ? "smelt-storage-reader" : "smelt-storage-reader-${var.namespace}"
}

resource "kubernetes_cluster_role_v1" "storage_reader" {
  count = var.storage_reader_enabled ? 1 : 0

  metadata {
    name = local.storage_reader_name
  }

  rule {
    api_groups = [""]
    resources  = ["persistentvolumes"]
    verbs      = ["get", "list"]
  }

  rule {
    api_groups = ["storage.k8s.io"]
    resources  = ["storageclasses"]
    verbs      = ["get", "list"]
  }
}

resource "kubernetes_cluster_role_binding_v1" "storage_reader" {
  count = var.storage_reader_enabled ? 1 : 0

  metadata {
    name = local.storage_reader_name
  }

  subject {
    kind      = "ServiceAccount"
    name      = kubernetes_service_account_v1.park.metadata[0].name
    namespace = kubernetes_namespace_v1.park.metadata[0].name
  }

  role_ref {
    api_group = "rbac.authorization.k8s.io"
    kind      = "ClusterRole"
    name      = kubernetes_cluster_role_v1.storage_reader[0].metadata[0].name
  }
}

# The ceiling on `create_pod`'s per-pod memory_limit override. No `cpu` entry
# is ever set here: it would hand every container a defaulted CPU limit and
# request it can't be scheduled with (SME-77).
#
# `default` and `defaultRequest` are declared, not omitted (SME-335): the API
# server fills a LimitRange item's unset defaults from `max` on every write,
# so a config that leaves them out can never converge — homelab's master sat
# permanently plan-dirty on `default -> null`. And the filled request was
# worse than drift: every container naming no memory of its own — the
# server's own `database-is-ready` init container among them — reserved the
# whole ceiling, which no node under it could schedule.
#
# `default` mirrors the max exactly: an un-capped container's limit stays
# what it has always effectively been, and lowering `sandbox_pod_memory_max`
# moves both together, so lowering the ceiling keeps applying.
# `defaultRequest` is zero: naming nothing reserves nothing — the SME-77
# intent, extended to containers that name nothing. Containers that DO name
# their memory — every container smelt builds, `requests.memory: 0` included
# — are untouched by either field.
resource "kubernetes_limit_range_v1" "sandbox_pod_max" {
  for_each = local.sandbox_namespaces

  metadata {
    name      = "sandbox-pod-max"
    namespace = each.value
  }

  spec {
    limit {
      type = "Container"
      max  = { memory = var.sandbox_pod_memory_max }

      default         = { memory = var.sandbox_pod_memory_max }
      default_request = { memory = "0" }
    }
  }
}
