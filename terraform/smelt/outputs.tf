output "namespace" {
  description = "The namespace smelt, its database and its sandboxes all live in."
  value       = kubernetes_namespace_v1.park.metadata[0].name
}

output "server_service" {
  description = "Namespace/name of the service in front of the server, for wiring something else to it."
  value       = "${kubernetes_deployment_v1.smelt.metadata[0].namespace}/${kubernetes_service_v1.smelt.metadata[0].name}"
}

output "ingress" {
  description = <<-EOT
    The ingress itself, not just its id. A consumer that wants to point DNS at it
    needs the object: homelab's pi-hole-service module, for one, reads the load
    balancer address out of `status`.
  EOT
  value       = kubernetes_ingress_v1.smelt
}

output "ingress_id" {
  description = "ID of the ingress this module creates (`namespace/name`), for a DNS record or a certificate to point at. The consumer of this module — not the module — decides what DNS says about it."
  value       = kubernetes_ingress_v1.smelt.id
}

output "url" {
  description = "Where the app says it lives (`SMELT_BASE_URL`)."
  value       = local.base_url
}

output "previews_enabled" {
  description = "Whether preview host names are published. False means the preview listener is bound to loopback inside the pod."
  value       = local.preview_enabled
}

output "database" {
  description = "The connection string in use: the external one given, or the one built for the Postgres deployed here. Sensitive, and stored in the cluster secret `smelt-db`."
  value       = local.database_url
  sensitive   = true
}

output "database_managed" {
  description = "True when this module deployed the Postgres itself, false when it was handed one."
  value       = local.create_database
}

output "sandbox_image" {
  description = "The image sandbox pods are created from, and the policy they pull it with."
  value       = { image = var.sandbox_image, pull_policy = var.image_pull_policy }
}

output "sandbox_pod_memory_max" {
  description = "The LimitRange ceiling on a sandbox pod's memory, per namespace."
  value       = { for ns, l in kubernetes_limit_range_v1.sandbox_pod_max : ns => l.metadata[0].name }
}
