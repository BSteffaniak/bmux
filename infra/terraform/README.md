# Cloudflare Infrastructure

This directory provisions the Cloudflare resources used for bmux package
distribution at `packages.bmux.dev`.

## Managed resources

- R2 bucket for all distribution channels
- DNS record for `packages.bmux.dev`

## Inputs

- `cloudflare_api_token`
- `cloudflare_account_id`
- `cloudflare_zone_id`
- `packages_hostname` (default: `packages`)
- `domain_name` (default: `bmux.dev`)
- `packages_bucket_name` (default: `bmux-packages`)
- `worker_cname_target` (for example, `bmux-packages.<subdomain>.workers.dev`)

## Example

```bash
tofu init
tofu plan \
  -var="cloudflare_api_token=$CLOUDFLARE_API_TOKEN" \
  -var="cloudflare_account_id=$CLOUDFLARE_ACCOUNT_ID" \
  -var="cloudflare_zone_id=$CLOUDFLARE_ZONE_ID" \
  -var="worker_cname_target=$WORKER_CNAME_TARGET"
tofu apply
```

## Notes

- Wrangler deploys the Worker itself. OpenTofu handles base infrastructure.
- Channel data is stored as prefixes in a single bucket: `stable/*` and `nightly/*`.
- Use the same account and zone values in CI release workflows.
- `worker_cname_target` should be your deployed Worker hostname, for example
  `bmux-packages.<subdomain>.workers.dev`.
- This setup is separate from the docs Pages project and should not collide with
  docs deployment.
