#!/usr/bin/env bash
# Shared durable backend initialization; no migration or provider mutations.
set -euo pipefail
umask 077
: "${CLOUDFLARE_ACCOUNT_ID:?}" "${LIVE_STATE_BUCKET:?}"
: "${R2_STATE_ACCESS_KEY_ID:?}" "${R2_STATE_SECRET_ACCESS_KEY:?}"
: "${TOFU_STATE_ENCRYPTION_PASSPHRASE:?}"
export AWS_ACCESS_KEY_ID="$R2_STATE_ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="$R2_STATE_SECRET_ACCESS_KEY"
export AWS_EC2_METADATA_DISABLED=true
export TF_VAR_state_encryption_passphrase="$TOFU_STATE_ENCRYPTION_PASSPHRASE"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BACKEND="$(mktemp)"
trap 'rm -f "$BACKEND"' EXIT
node - "$BACKEND" <<'JS'
const fs = require('node:fs');
const { CLOUDFLARE_ACCOUNT_ID: account, LIVE_STATE_BUCKET: bucket } = process.env;
if (!/^[a-f0-9]{32}$/.test(account) || !/^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$/.test(bucket))
  throw new Error('Invalid state backend identity');
fs.writeFileSync(process.argv[2], JSON.stringify({
  bucket, region: 'auto', key: 'bmux/github/production/terraform.tfstate',
  endpoint: `https://${account}.r2.cloudflarestorage.com`, use_lockfile: true,
  skip_credentials_validation: true, skip_metadata_api_check: true,
  skip_region_validation: true, skip_requesting_account_id: true, skip_s3_checksum: true,
}));
JS
tofu -chdir="$ROOT/infra/live-github" init -input=false -lockfile=readonly -backend-config="$BACKEND"
