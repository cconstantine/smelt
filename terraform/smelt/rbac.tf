# What a sandbox needs from the cluster, and what smelt needs to be able to
# ask for it. This is k8s/smelt-park-rbac.yaml expressed in Terraform: the same
# `park` ServiceAccount, the same two Role/RoleBinding pairs, the same LimitRange
# ceiling on a sandbox pod's memory. Keeping it here means a deployment gets the
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

# The ceiling on `create_pod`'s per-pod memory_limit override. Only `max` is
# set: a `cpu` entry here would hand every container a defaulted CPU limit and
# request it can't be scheduled with (SME-77).
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
    }
  }
}
