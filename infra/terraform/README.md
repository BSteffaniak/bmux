# Cloudflare Infrastructure

This directory provisions the Cloudflare resources used for bmux package
distribution at `packages.bmux.dev`.

## Managed resources

- R2 bucket for stable channel artifacts and repositories
- R2 bucket for nightly channel artifacts and repositories
- DNS record for `packages.bmux.dev`

## Inputs

- `cloudflare_api_token`
- `cloudflare_account_id`
- `cloudflare_zone_id`
- `packages_hostname` (default: `packages`)
- `domain_name` (default: `bmux.dev`)

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
- Use the same account and zone values in CI release workflows.
