terraform {
  required_version = "= 1.10.9"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "= 5.100.0"
    }
  }
  # Bootstrap starts with LOCAL state; back it up securely. This is not live state.
}
variable "region" { type = string }
variable "bucket_prefix" { type = string }
variable "environments" {
  type    = set(string)
  default = ["staging", "production"]
  validation {
    condition     = length(var.environments) > 0 && length(setsubtract(var.environments, toset(["staging", "production"]))) == 0
    error_message = "Only explicit staging/production environments are supported."
  }
}
variable "existing_buckets" {
  type    = map(string)
  default = {}
}
variable "github_repository" {
  type    = string
  default = ""
  validation {
    condition     = var.github_repository == "" || can(regex("^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$", var.github_repository))
    error_message = "Supply one exact GitHub owner/repository."
  }
}
variable "github_oidc_provider_arn" {
  type    = string
  default = ""
}
variable "github_role_name" {
  type    = string
  default = "bmux-live-state-production"
}
provider "aws" { region = var.region }
locals {
  oidc_enabled = var.github_repository != ""
  provider_arn = var.github_oidc_provider_arn != "" ? var.github_oidc_provider_arn : try(aws_iam_openid_connect_provider.github[0].arn, "")
}
resource "aws_iam_openid_connect_provider" "github" {
  count          = local.oidc_enabled && var.github_oidc_provider_arn == "" ? 1 : 0
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
  lifecycle { prevent_destroy = true }
}
resource "aws_iam_role" "github_production" {
  count                = local.oidc_enabled ? 1 : 0
  name                 = var.github_role_name
  max_session_duration = 3600
  assume_role_policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Effect    = "Allow", Action = "sts:AssumeRoleWithWebIdentity",
    Principal = { Federated = local.provider_arn },
    Condition = { StringEquals = {
      "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com",
      "token.actions.githubusercontent.com:sub" = "repo:${var.github_repository}:environment:Production:ref:refs/heads/master"
    } }
  }] })
  lifecycle {
    prevent_destroy = true
    precondition {
      condition     = contains(var.environments, "production")
      error_message = "OIDC role requires the production backend."
    }
  }
}
resource "aws_iam_role_policy_attachment" "github_production" {
  count      = local.oidc_enabled ? 1 : 0
  role       = aws_iam_role.github_production[0].name
  policy_arn = aws_iam_policy.state["production"].arn
}
resource "aws_s3_bucket" "state" {
  for_each = var.environments
  bucket   = lookup(var.existing_buckets, each.key, "${var.bucket_prefix}-${each.key}")
  lifecycle { prevent_destroy = true }
}
resource "aws_s3_bucket_versioning" "state" {
  for_each = aws_s3_bucket.state
  bucket   = each.value.id
  versioning_configuration { status = "Enabled" }
}
resource "aws_s3_bucket_server_side_encryption_configuration" "state" {
  for_each = aws_s3_bucket.state
  bucket   = each.value.id
  rule {
    apply_server_side_encryption_by_default { sse_algorithm = "AES256" }
  }
}
resource "aws_s3_bucket_public_access_block" "state" {
  for_each                = aws_s3_bucket.state
  bucket                  = each.value.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}
resource "aws_s3_bucket_policy" "tls" {
  for_each = aws_s3_bucket.state
  bucket   = each.value.id
  policy = jsonencode({ Version = "2012-10-17", Statement = [{
    Effect    = "Deny", Principal = "*", Action = "s3:*",
    Resource  = [each.value.arn, "${each.value.arn}/*"],
    Condition = { Bool = { "aws:SecureTransport" = "false" } }
  }] })
}
resource "aws_iam_policy" "state" {
  for_each = aws_s3_bucket.state
  name     = "bmux-live-state-${each.key}"
  policy = jsonencode({ Version = "2012-10-17", Statement = [
    { Effect = "Allow", Action = ["s3:GetBucketLocation", "s3:GetBucketVersioning", "s3:GetBucketPublicAccessBlock", "s3:GetEncryptionConfiguration"], Resource = each.value.arn },
    { Effect = "Allow", Action = ["s3:ListBucket"], Resource = each.value.arn,
    Condition = { StringLike = { "s3:prefix" = ["bmux/live/${each.key}/*"] } } },
    { Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject"],
    Resource = "${each.value.arn}/bmux/live/${each.key}/terraform.tfstate" },
    { Effect = "Allow", Action = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
    Resource = "${each.value.arn}/bmux/live/${each.key}/terraform.tfstate.tflock" }
  ] })
}
# Do not create long-lived access keys in state. Attach these policies to separately
# managed environment identities; operator provides their credentials privately.
output "github_role_arn" { value = try(aws_iam_role.github_production[0].arn, null) }
output "github_oidc_subject" { value = local.oidc_enabled ? "repo:${var.github_repository}:environment:Production:ref:refs/heads/master" : null }
output "buckets" { value = { for env, bucket in aws_s3_bucket.state : env => bucket.id } }
output "state_policy_arns" { value = { for env, policy in aws_iam_policy.state : env => policy.arn } }
