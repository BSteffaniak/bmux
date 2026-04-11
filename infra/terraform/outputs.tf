output "stable_bucket_name" {
  value = cloudflare_r2_bucket.stable.name
}

output "nightly_bucket_name" {
  value = cloudflare_r2_bucket.nightly.name
}

output "packages_fqdn" {
  value = local.packages_fqdn
}
