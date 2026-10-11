# A throwaway root that runs the smelt module against a k3d cluster. Its only
# job is to prove the module applies and boots; nothing about it is what a real
# deployment should look like (see terraform/smelt/README.md for that, and the
# homelab repo for a consumer).
#
#   k3d cluster create smelt-test --image rancher/k3s:v1.34.6-k3s1 \
#     --port "18080:80@loadbalancer" --wait
#   terraform init && terraform apply
#
# The k3s image matters: sandbox pods use native sidecar containers, which a
# k3d default image (1.28-era) refuses. Pin the one docs/setup.md pins.
#
# Sandbox/server images can't be pulled from GHCR inside the node (the registry
# is unreachable from the k3d network on some sandboxes), so import them into
# the node's containerd before the first pod start:
#
#   docker cp <image>.tar k3d-smelt-test-server-0:/tmp/
#   docker exec k3d-smelt-test-server-0 \
#     ctr --address /run/k3s/containerd/containerd.sock -n k8s.io images import /tmp/<image>.tar
#
# The `0.0.0.0` k3d puts in its kubeconfig is not something the provider can
# verify a certificate against, so the kubeconfig used here has that rewritten
# to loopback.

terraform {
  required_version = ">= 1.5.0"

  required_providers {
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 3.0"
    }
  }
}

variable "kubeconfig_path" {
  description = "Kubeconfig for the test cluster."
  type        = string
  default     = "~/.kube/config"
}

variable "kubeconfig_context" {
  description = "Context to use within it."
  type        = string
  default     = "k3d-smelt-test"
}

variable "image" {
  description = "Server image to deploy."
  type        = string
  default     = "ghcr.io/cconstantine/smelt:c0a089794fc7"
}

variable "sandbox_image" {
  description = "Sandbox agent image pods get created from."
  type        = string
  default     = "ghcr.io/cconstantine/smelt-sandbox:c0a089794fc7"
}

provider "kubernetes" {
  config_path    = var.kubeconfig_path
  config_context = var.kubeconfig_context
}

module "smelt" {
  source = "../smelt"

  image         = var.image
  sandbox_image = var.sandbox_image

  # Anything Traefik will route by Host header. `*.localhost` resolves to this
  # machine everywhere that matters here.
  domain = "smelt.k3d.localhost"

  # The module's own test namespace, so this cluster can also run smelt's
  # real-cluster tests.
  test_namespace = true

  # k3s's default provisioner, named explicitly so the plan says which storage
  # the Postgres claim lands on.
  storage_class_name = "local-path"
}

output "url" {
  value     = module.smelt.url
  sensitive = false
}

output "ingress_id" {
  value = module.smelt.ingress_id
}

output "namespace" {
  value = module.smelt.namespace
}
