variable "cloudflare_api_token" {
  description = "Cloudflare API token"
  type        = string
  sensitive   = true
}

variable "cloudflare_account_id" {
  description = "Cloudflare account id"
  type        = string
}

variable "cloudflare_zone_id" {
  description = "Cloudflare zone id for bmux.dev"
  type        = string
}

variable "packages_hostname" {
  description = "Host label for package endpoint"
  type        = string
  default     = "packages"
}

variable "domain_name" {
  description = "Apex domain"
  type        = string
  default     = "bmux.dev"
}

variable "stable_bucket_name" {
  description = "R2 bucket for stable channel"
  type        = string
  default     = "bmux-packages-stable"
}

variable "nightly_bucket_name" {
  description = "R2 bucket for nightly channel"
  type        = string
  default     = "bmux-packages-nightly"
}

variable "worker_cname_target" {
  description = "Target hostname for packages CNAME (for example, <worker-subdomain>.workers.dev)"
  type        = string
}
