# Exactly one authority process; no autoscaler and no create-before-destroy.
variable "fly_app_name" { type = string }
variable "fly_org" { type = string }
variable "fly_region" {
  type    = string
  default = "iad"
}
variable "relay_hostname" {
  type    = string
  default = "live.bmux.dev"
}
variable "metrics_key_sha256" {
  type      = string
  sensitive = true
  validation {
    condition     = can(regex("^[0-9a-f]{64}$", var.metrics_key_sha256))
    error_message = "Metrics key must be a lowercase SHA-256 digest."
  }
}
variable "allowed_origins" {
  type    = string
  default = "https://bmux.dev,https://www.bmux.dev"
}
variable "relay_image" {
  type = string
  validation {
    condition     = can(regex("^[a-zA-Z0-9./:_-]+@sha256:[0-9a-f]{64}$", var.relay_image))
    error_message = "Relay image must be a registry reference pinned by sha256 digest."
  }
}
resource "fly_app" "live" {
  name = var.fly_app_name
  org  = var.fly_org
  lifecycle { prevent_destroy = true }
}
resource "fly_ip" "v4" {
  app  = fly_app.live.name
  type = "shared_v4"
}
resource "fly_ip" "v6" {
  app  = fly_app.live.name
  type = "public_v6"
}
# Private authority metadata only: no terminal payloads or session history.
resource "fly_volume" "authority" {
  app       = fly_app.live.name
  name      = "live_authority"
  region    = var.fly_region
  size_gb   = 1
  encrypted = true
  lifecycle { prevent_destroy = true }
}
resource "fly_machine" "live" {
  app    = fly_app.live.name
  name   = "relay"
  region = var.fly_region
  image  = var.relay_image
  guest {
    cpu_kind  = "shared"
    cpus      = 1
    memory_mb = 1024
  }
  env = {
    PUBLISHER_KEY_SHA256 = var.publisher_key_sha256
    METRICS_KEY_SHA256   = var.metrics_key_sha256
    ALLOWED_ORIGINS      = var.allowed_origins
    AUTHORITY_PATH       = "/data/relay/authority.json"
  }
  mount {
    volume = fly_volume.authority.id
    path   = "/data"
  }
  restart { policy = "always" }
  # /metrics is authenticated by METRICS_KEY_SHA256. Do not configure Fly's
  # unauthenticated scraper; an authorized private scraper supplies the key.
  service {
    internal_port = 8080
    protocol      = "tcp"
    autostart     = true
    autostop      = "off"
    port {
      port     = 443
      handlers = ["tls", "http"]
    }
    check {
      type     = "http"
      port     = 8080
      path     = "/health"
      interval = "10s"
      timeout  = "2s"
    }
  }
  depends_on = [fly_ip.v4, fly_ip.v6]
}
resource "fly_cert" "live" {
  app      = fly_app.live.name
  hostname = var.relay_hostname
}
resource "cloudflare_dns_record" "relay" {
  zone_id = var.zone_id
  name    = var.relay_hostname
  type    = "CNAME"
  content = fly_cert.live.cname
  proxied = false
  ttl     = 60
  lifecycle { prevent_destroy = true }
}
resource "cloudflare_dns_record" "relay_acme" {
  zone_id = var.zone_id
  name    = fly_cert.live.acme_challenge_name
  type    = "CNAME"
  # Fly returns an absolute DNS name; Cloudflare stores CNAMEs without the
  # terminal dot. Normalize configuration to avoid a perpetual update plan.
  content = trimsuffix(fly_cert.live.acme_challenge_target, ".")
  proxied = false
  ttl     = 60
}
resource "fly_cert_validation" "live" {
  cert_id    = fly_cert.live.id
  depends_on = [cloudflare_dns_record.relay, cloudflare_dns_record.relay_acme]
}
