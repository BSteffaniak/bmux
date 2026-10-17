terraform {
  required_version = "= 1.10.9"
  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = "= 5.17.0"
    }
    fly = {
      # Community-maintained Machines API provider; not archived fly-apps/fly.
      source  = "registry.terraform.io/ampbase-io/fly"
      version = "= 0.3.2"
    }
  }
  backend "s3" {}
}

provider "cloudflare" {} # CLOUDFLARE_API_TOKEN; never tokens in tfvars.
provider "fly" {
  org_slug = var.fly_org
} # FLY_API_TOKEN; never tokens in tfvars.

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
    error_message = "Supply an exact Pages hostname, not a wildcard or URL."
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
  zone_id = var.zone_id
  name    = var.hostname
  type    = "CNAME"
  content = cloudflare_pages_project.live.subdomain
  proxied = true
  ttl     = 1
  lifecycle { prevent_destroy = true }
}
output "health_url" { value = "https://${var.relay_hostname}/health" }
