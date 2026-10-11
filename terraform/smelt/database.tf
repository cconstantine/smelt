# smelt's database. Either an external one the caller names, or — the default —
# a Postgres of smelt's own in its namespace, with a claim of its own. It lives
# in `namespace` (not its own) so the `park` Role is enough to reach it and so
# deleting the namespace takes it with everything else smelt owns.
#
# smelt applies its own migrations at startup, so nothing here pre-creates
# schema; it only needs the database to exist and to answer.

locals {
  create_database = var.deploy_database

  db_service_name = "smelt-db"
  db_user         = "postgres"

  # Exactly one source: the Postgres deployed here, or the caller's own URL.
  # Which one is decided by `deploy_database`, never read off `database_url` —
  # see that variable's description for why a sensitive value can't decide.
  database_url = var.deploy_database ? "postgres://${local.db_user}:${random_password.database[0].result}@${kubernetes_service_v1.database[0].metadata[0].name}.${kubernetes_namespace_v1.park.metadata[0].name}.svc.cluster.local/${var.database_name}" : var.database_url
}

resource "random_password" "database" {
  count = local.create_database ? 1 : 0

  length  = 24
  special = false

  # Alphanumeric only, because this value goes into a connection URL that
  # nothing here percent-encodes.
  keepers = {
    namespace = kubernetes_namespace_v1.park.metadata[0].name
  }
}

resource "kubernetes_secret_v1" "database" {
  metadata {
    name      = "smelt-db"
    namespace = kubernetes_namespace_v1.park.metadata[0].name

    labels = {
      "app.kubernetes.io/name" = "smelt"
    }
  }

  data = local.create_database ? {
    DATABASE_URL      = local.database_url
    POSTGRES_PASSWORD = random_password.database[0].result
    } : {
    DATABASE_URL = local.database_url
  }

  type = "Opaque"

  # Exactly one database source. Checking it here rather than in a variable
  # validation, because a validation condition may not be built from a sensitive
  # variable's value while this can.
  lifecycle {
    precondition {
      condition     = var.deploy_database == (var.database_url == null)
      error_message = "Give smelt exactly one database: leave deploy_database true for the Postgres deployed here, or set it false and supply database_url."
    }
  }
}

resource "kubernetes_service_v1" "database" {
  count = local.create_database ? 1 : 0

  metadata {
    name      = local.db_service_name
    namespace = kubernetes_namespace_v1.park.metadata[0].name

    labels = {
      "app.kubernetes.io/name" = "smelt"
    }
  }

  spec {
    selector = {
      "app.kubernetes.io/name"      = "smelt"
      "app.kubernetes.io/component" = "database"
    }

    port {
      name        = "postgres"
      port        = 5432
      target_port = 5432
    }
  }
}

resource "kubernetes_stateful_set_v1" "database" {
  count = local.create_database ? 1 : 0

  metadata {
    name      = local.db_service_name
    namespace = kubernetes_namespace_v1.park.metadata[0].name

    labels = {
      "app.kubernetes.io/name"      = "smelt"
      "app.kubernetes.io/component" = "database"
    }
  }

  spec {
    replicas     = 1
    service_name = local.db_service_name

    selector {
      match_labels = {
        "app.kubernetes.io/name"      = "smelt"
        "app.kubernetes.io/component" = "database"
      }
    }

    template {
      metadata {
        labels = {
          "app.kubernetes.io/name"      = "smelt"
          "app.kubernetes.io/component" = "database"
        }
      }

      spec {
        # The claim lands on whatever node takes the pod; fsGroup is what makes
        # it writable by the postgres user (uid 5432 in the image) instead of
        # root-owned. run_as_user deliberately not set: the image's entrypoint
        # starts as root and drops privileges itself.
        security_context {
          fs_group = "5432"
        }

        container {
          name  = "postgres"
          image = var.postgres_image

          port {
            name           = "postgres"
            container_port = 5432
          }

          env {
            name = "POSTGRES_PASSWORD"

            value_from {
              secret_key_ref {
                name = kubernetes_secret_v1.database.metadata[0].name
                key  = "POSTGRES_PASSWORD"
              }
            }
          }

          env {
            name  = "POSTGRES_USER"
            value = local.db_user
          }

          env {
            name  = "POSTGRES_DB"
            value = var.database_name
          }

          # PGDATA has to be a subdirectory of the mount, or initdb refuses the
          # lost+found the filesystem brings with it.
          env {
            name  = "PGDATA"
            value = "/var/lib/postgresql/data/pgdata"
          }

          volume_mount {
            name       = "data"
            mount_path = "/var/lib/postgresql/data"
          }

          readiness_probe {
            exec {
              command = ["pg_isready", "-U", local.db_user]
            }

            initial_delay_seconds = 5
            period_seconds        = 5
          }

          liveness_probe {
            exec {
              command = ["pg_isready", "-U", local.db_user]
            }

            initial_delay_seconds = 30
            period_seconds        = 30
          }

          resources {
            requests = {
              memory = "256Mi"
              cpu    = "100m"
            }

            limits = {
              memory = "1Gi"
            }
          }
        }
      }
    }

    volume_claim_template {
      metadata {
        name = "data"
      }

      spec {
        access_modes       = ["ReadWriteOnce"]
        storage_class_name = var.storage_class_name

        resources {
          requests = {
            storage = var.database_storage_size
          }
        }
      }
    }
  }
}
