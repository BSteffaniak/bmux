terraform {
  required_version = "= 1.10.9"
  required_providers {
    github = {
      source  = "integrations/github"
      version = "= 6.6.0"
    }
  }
  # Dedicated encrypted remote state; never share the live application's key.
}

provider "github" {
  owner = var.owner
} # GITHUB_TOKEN comes from the operator environment, never tfvars.

variable "owner" { type = string }
variable "repository" { type = string }
variable "approval_reviewer" { type = string }
variable "deployment_environment" {
  type    = string
  default = "Production"
  validation {
    condition     = var.deployment_environment == "Production"
    error_message = "Release workflows use the exact environment name Production."
  }
}
variable "existing_environments" {
  description = "Existing environments to adopt explicitly, not recreate."
  type        = set(string)
  default     = []
  validation {
    condition     = length(setsubtract(var.existing_environments, ["Production", "production-approval"])) == 0
    error_message = "Only the two release environments may be adopted."
  }
}
variable "existing_master_policy_ids" {
  description = "Existing master branch policy IDs keyed by environment; for explicit adoption."
  type        = map(number)
  default     = {}
  validation {
    condition     = length(setsubtract(toset(keys(var.existing_master_policy_ids)), ["Production", "production-approval"])) == 0
    error_message = "Only the two release environments may be adopted."
  }
}

data "github_user" "reviewer" {
  username = var.approval_reviewer
}

resource "github_repository_environment" "release" {
  for_each = toset([var.deployment_environment, "production-approval"])

  repository          = var.repository
  environment         = each.key
  wait_timer          = 0
  prevent_self_review = false
  can_admins_bypass   = false
  # Only the approval-only environment requires review. Credential-bearing jobs
  # run after that gate and must not request another human approval.
  dynamic "reviewers" {
    for_each = each.key == "production-approval" ? [1] : []
    content {
      users = [tonumber(data.github_user.reviewer.id)]
    }
  }
  deployment_branch_policy {
    protected_branches     = false
    custom_branch_policies = true
  }
  lifecycle { prevent_destroy = true }
}

resource "github_repository_environment_deployment_policy" "master" {
  for_each       = github_repository_environment.release
  repository     = var.repository
  environment    = each.value.environment
  branch_pattern = "master"
  lifecycle { prevent_destroy = true }
}

import {
  for_each = var.existing_environments
  to       = github_repository_environment.release[each.key]
  id       = "${var.repository}:${each.key}"
}
import {
  for_each = var.existing_master_policy_ids
  to       = github_repository_environment_deployment_policy.master[each.key]
  id       = "${var.repository}:${each.key}:${each.value}"
}
