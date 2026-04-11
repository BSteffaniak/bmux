output "packages_bucket_name" {
  value = cloudflare_r2_bucket.packages.name
}

output "packages_fqdn" {
  value = local.packages_fqdn
}
