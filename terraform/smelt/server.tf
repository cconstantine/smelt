# The smelt server: one Deployment, one Service, one Ingress.
#
# The pod runs as the `park` ServiceAccount, which is how it reaches the cluster:
# kube's client infers in-cluster credentials from the pod's own token, so no
# KUBECONFIG is mounted and none is needed. That's what makes the RBAC in
# rbac.tf the whole story about what smelt may do.
#
# Recreate rather than the default RollingUpdate, because a new server applies
# its migrations at startup: an old pod and a new one running that at the same
# moment is two writers against the same schema.

locals {
  base_url = coalesce(var.base_url, "https://${var.domain}")

  allowed_hosts = join(",", distinct(concat([var.domain], var.additional_hosts)))

  preview_enabled = var.preview_domain != null
  preview_url     = local.preview_enabled ? "https://{port}-{conversation}.${var.preview_domain}" : null

  # Every non-secret environment value, with the ones left unset dropped — smelt's
  # own defaults are what should apply unless the caller asked otherwise.
  server_env = {
    for key, value in {
      SMELT_BASE_URL                    = local.base_url
      SMELT_ALLOWED_HOSTS               = local.allowed_hosts
      SMELT_PREVIEW_ADDR                = local.preview_enabled ? "0.0.0.0:8181" : "127.0.0.1:8181"
      SMELT_PREVIEW_URL                 = local.preview_url
      SANDBOX_IMAGE                     = var.sandbox_image
      SANDBOX_IMAGE_PULL_POLICY         = var.image_pull_policy
      SANDBOX_IMAGE_PULL_SECRET         = var.image_pull_secret
      SANDBOX_MEMORY_LIMIT              = var.sandbox_memory_limit
      SANDBOX_DOCKER_STORAGE_SIZE       = var.docker_storage_size
      SANDBOX_WORKSPACE_STORAGE_SIZE    = var.workspace_storage_size
      SANDBOX_RUNNING_WAIT_TIMEOUT_SECS = var.sandbox_wait_timeout_secs != null ? tostring(var.sandbox_wait_timeout_secs) : null
    } :
    key => value if value != null
  }

  env = merge(var.extra_env, local.server_env)
}

resource "kubernetes_deployment_v1" "smelt" {
  metadata {
    name      = "smelt"
    namespace = kubernetes_namespace_v1.park.metadata[0].name

    labels = {
      "app.kubernetes.io/name" = "smelt"
    }
  }

  spec {
    replicas = 1

    strategy {
      type = "Recreate"
    }

    selector {
      match_labels = {
        "app.kubernetes.io/name" = "smelt"
      }
    }

    template {
      metadata {
        labels = {
          "app.kubernetes.io/name" = "smelt"
        }
      }

      spec {
        service_account_name = kubernetes_service_account_v1.park.metadata[0].name

        dynamic "image_pull_secrets" {
          for_each = var.image_pull_secret != null ? toset([var.image_pull_secret]) : toset([])

          content {
            name = image_pull_secrets.value
          }
        }

        # The image ships no volumes: everything the server keeps lives in
        # Postgres. Nothing to mount.
        dynamic "init_container" {
          for_each = local.create_database ? toset(["database-is-ready"]) : toset([])

          content {
            name  = "database-is-ready"
            image = var.postgres_image

            # pg_isready alone, until the database answers or a couple of
            # minutes go by. Failing out is the point: a server that starts
            # before its database would migrate against nothing.
            command = [
              "sh", "-c",
              <<-EOT
              tries=0
              until pg_isready -h ${local.db_service_name}.${kubernetes_namespace_v1.park.metadata[0].name}.svc.cluster.local -U ${local.db_user}; do
                tries=$((tries + 1))
                if [ "$tries" -gt 40 ]; then
                  echo "the database never answered" >&2
                  exit 1
                fi
                sleep 3
              done
              EOT
            ]
          }
        }

        container {
          name              = "smelt"
          image             = var.image
          image_pull_policy = var.image_pull_policy

          port {
            name           = "http"
            container_port = 8080
          }

          port {
            name           = "previews"
            container_port = 8181
          }

          env {
            name = "DATABASE_URL"

            value_from {
              secret_key_ref {
                name = kubernetes_secret_v1.database.metadata[0].name
                key  = "DATABASE_URL"
              }
            }
          }

          dynamic "env" {
            for_each = local.env

            content {
              name  = env.key
              value = env.value
            }
          }

          # /api/build-id answers as soon as the app is up, with no auth and
          # no database work of its own.
          readiness_probe {
            http_get {
              path = "/api/build-id"
              port = "http"
            }

            initial_delay_seconds = 5
            period_seconds        = 10
          }

          liveness_probe {
            http_get {
              path = "/api/build-id"
              port = "http"
            }

            initial_delay_seconds = 30
            period_seconds        = 30
          }

          resources {
            requests = {
              cpu    = var.server_cpu
              memory = var.server_memory
            }

            limits = {
              memory = var.server_memory
            }
          }
        }
      }
    }
  }
}

resource "kubernetes_service_v1" "smelt" {
  metadata {
    name      = "smelt"
    namespace = kubernetes_namespace_v1.park.metadata[0].name

    labels = {
      "app.kubernetes.io/name" = "smelt"
    }
  }

  spec {
    selector = {
      "app.kubernetes.io/name" = "smelt"
    }

    port {
      name        = "http"
      port        = 8080
      target_port = "http"
    }

    # The preview listener is always up (it answers the pod's own ports too);
    # it's only ever *published* when a preview domain was given.
    port {
      name        = "previews"
      port        = 8181
      target_port = "previews"
    }
  }
}

resource "kubernetes_ingress_v1" "smelt" {
  metadata {
    name      = "smelt"
    namespace = kubernetes_namespace_v1.park.metadata[0].name

    labels = {
      "app.kubernetes.io/name" = "smelt"
    }

    annotations = length(var.ingress_annotations) > 0 ? var.ingress_annotations : null
  }

  spec {
    ingress_class_name = var.ingress_class_name

    rule {
      host = var.domain

      http {
        path {
          path      = "/"
          path_type = "Prefix"

          backend {
            service {
              name = kubernetes_service_v1.smelt.metadata[0].name

              port {
                number = 8080
              }
            }
          }
        }
      }
    }

    dynamic "rule" {
      for_each = local.preview_enabled ? toset(["*.${var.preview_domain}"]) : toset([])

      content {
        host = rule.value

        http {
          path {
            path      = "/"
            path_type = "Prefix"

            backend {
              service {
                name = kubernetes_service_v1.smelt.metadata[0].name

                port {
                  number = 8181
                }
              }
            }
          }
        }
      }
    }
  }
}
