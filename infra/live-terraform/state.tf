variable "state_bucket_name" {
  type        = string
  description = "Private R2 bucket used for encrypted OpenTofu state"
}

variable "state_archive_retention_days" {
  type        = number
  default     = 365
  description = "Days to retain immutable encrypted OpenTofu state revisions under history/"

  validation {
    condition     = var.state_archive_retention_days >= 90
    error_message = "state_archive_retention_days must be at least 90 days."
  }
}

resource "cloudflare_r2_bucket" "state" {
  account_id    = var.account_id
  name          = var.state_bucket_name
  storage_class = "Standard"

  lifecycle {
    prevent_destroy = true
  }
}

resource "cloudflare_r2_bucket_lock" "state" {
  account_id  = var.account_id
  bucket_name = cloudflare_r2_bucket.state.name

  rules = [{
    id      = "retain-opentofu-state-history"
    enabled = true
    prefix  = "history/"
    condition = {
      type            = "Age"
      max_age_seconds = var.state_archive_retention_days * 86400
    }
  }]
}

resource "cloudflare_r2_bucket_lifecycle" "state" {
  account_id  = var.account_id
  bucket_name = cloudflare_r2_bucket.state.name

  rules = [
    {
      id      = "expire-old-opentofu-state-history"
      enabled = true
      conditions = {
        prefix = "history/"
      }
      delete_objects_transition = {
        condition = {
          type    = "Age"
          max_age = var.state_archive_retention_days * 86400
        }
      }
    },
    {
      # Preserve Cloudflare's pristine baseline rather than shorten its window.
      id      = "Default Multipart Abort Rule"
      enabled = true
      # Provider requires prefix; empty is the API's omitted all-objects prefix.
      conditions = {
        prefix = ""
      }
      abort_multipart_uploads_transition = {
        condition = {
          type    = "Age"
          max_age = 604800
        }
      }
    }
  ]
}
