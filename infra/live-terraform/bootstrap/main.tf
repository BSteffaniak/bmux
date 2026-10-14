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
provider "aws" { region = var.region }
resource "aws_s3_bucket" "state" {
  for_each = toset(["staging", "production"])
  bucket   = "${var.bucket_prefix}-${each.key}"
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
output "buckets" { value = { for env, bucket in aws_s3_bucket.state : env => bucket.id } }
output "state_policy_arns" { value = { for env, policy in aws_iam_policy.state : env => policy.arn } }
