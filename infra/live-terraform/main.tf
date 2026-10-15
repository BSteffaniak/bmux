terraform {
  required_version = "= 1.10.9"
  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = "= 5.17.0"
    }
  }
  backend "s3" {}
}

provider "cloudflare" {} # CLOUDFLARE_API_TOKEN; never place tokens in tfvars.

variable "environment" {
  type = string
  validation {
    condition     = contains(["staging", "production"], var.environment)
    error_message = "Only staging or production is supported."
  }
}
variable "account_id" { type = string }
variable "zone_id" { type = string }
variable "hostname" {
  type = string
  validation {
    condition     = can(regex("^[a-z0-9.-]+$", var.hostname))
    error_message = "Supply an exact DNS hostname, not a wildcard or URL."
  }
}
variable "publisher_key_sha256" {
  type      = string
  sensitive = true
  validation {
    condition     = can(regex("^[0-9a-f]{64}$", var.publisher_key_sha256))
    error_message = "Publisher key must be a lowercase SHA-256 digest."
  }
}
variable "relay_directory" { type = string }
variable "pages_project" {
  type = string
  validation {
    condition     = can(regex("^[a-z0-9-]{1,58}$", var.pages_project))
    error_message = "Supply the environment-specific Pages project name."
  }
}
resource "cloudflare_pages_project" "live" {
  account_id        = var.account_id
  name              = var.pages_project
  production_branch = "master"
  lifecycle { prevent_destroy = true }
}
resource "cloudflare_pages_domain" "live" {
  account_id   = var.account_id
  project_name = cloudflare_pages_project.live.name
  name         = var.hostname
  lifecycle { prevent_destroy = true }
}
resource "cloudflare_dns_record" "live" {
  zone_id    = var.zone_id
  name       = var.hostname
  type       = "CNAME"
  content    = "${cloudflare_pages_project.live.name}.pages.dev"
  ttl        = 1
  proxied    = true
  depends_on = [cloudflare_pages_domain.live]
  lifecycle { prevent_destroy = true }
}

resource "cloudflare_worker" "live" {
  account_id    = var.account_id
  name          = "bmux-live-${var.environment}"
  logpush       = false
  observability = { enabled = false }
  subdomain     = { enabled = false, previews_enabled = false }
  lifecycle { prevent_destroy = true }
}

variable "live_migration_only" {
  type        = bool
  default     = false
  description = "Configure-only first deployment stage: deploy class migration before referencing its binding"
}

resource "cloudflare_worker_version" "live" {
  account_id         = var.account_id
  worker_id          = cloudflare_worker.live.id
  main_module        = "worker.js"
  compatibility_date = "2026-01-01"
  modules = [for path in sort(fileset(var.relay_directory, "**/*.js")) : {
    name         = path
    content_file = "${var.relay_directory}/${path}"
    content_type = "application/javascript+module"
  }]
  bindings = [for binding in [
    { name = "LIVE_CHANNELS", type = "durable_object_namespace", class_name = "LiveChannel" },
    { name = "CHANNEL", type = "plain_text", text = "main" },
    { name = "PUBLISHER_KEY_SHA256", type = "plain_text", text = var.publisher_key_sha256 }
  ] : binding if !var.live_migration_only]
  migrations = var.live_migration_only ? { new_tag = "v1", new_sqlite_classes = ["LiveChannel"] } : null
}
resource "cloudflare_workers_deployment" "live" {
  account_id  = var.account_id
  script_name = cloudflare_worker.live.name
  # Deployment resources are immutable in pinned provider 5.17.0; Delete is
  # state-only. Activate the next deployment before retiring the old record.
  lifecycle { create_before_destroy = true }
  strategy = "percentage"
  versions = [{ percentage = 100, version_id = cloudflare_worker_version.live.id }]
}
resource "cloudflare_workers_route" "live" {
  zone_id    = var.zone_id
  pattern    = "${var.hostname}/api/live/*"
  script     = cloudflare_worker.live.name
  depends_on = [cloudflare_workers_deployment.live]
}
output "health_url" { value = "https://${var.hostname}/api/live/main/health" }
