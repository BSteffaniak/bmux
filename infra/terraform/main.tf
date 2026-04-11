provider "cloudflare" {
  api_token = var.cloudflare_api_token
}

locals {
  packages_fqdn = "${var.packages_hostname}.${var.domain_name}"
}

resource "cloudflare_r2_bucket" "packages" {
  account_id = var.cloudflare_account_id
  name       = var.packages_bucket_name
  location   = "WNAM"
}

resource "cloudflare_dns_record" "packages" {
  zone_id = var.cloudflare_zone_id
  name    = var.packages_hostname
  type    = "CNAME"
  content = var.worker_cname_target
  proxied = true
  ttl     = 1
}
