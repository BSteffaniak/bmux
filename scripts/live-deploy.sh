#!/usr/bin/env bash
# Credentialed operator/CI entry point. Never invoke during offline preparation.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENVIRONMENT="${1:?Usage: live-deploy.sh staging|production ARTIFACT_DIRECTORY}"
[[ "$ENVIRONMENT" == staging || "$ENVIRONMENT" == production ]] || exit 2
ARTIFACT="$(cd "${2:?Artifact directory required}" && pwd)"
: "${LIVE_STATE_ACCESS_KEY_ID:?Separate state IAM credential required}"
: "${LIVE_STATE_SECRET_ACCESS_KEY:?Separate state IAM credential required}"
: "${CLOUDFLARE_API_TOKEN:?Scoped deployment credential required}"
: "${CLOUDFLARE_ACCOUNT_ID:?}"
: "${CLOUDFLARE_ZONE_ID:?}"
: "${LIVE_HOSTNAME:?}"
: "${LIVE_PUBLISHER_KEY_SHA256:?Digest only; never publisher key}"
: "${LIVE_PAGES_PROJECT:?Environment-specific existing Pages project}"
[[ "$(tofu version -json | python3 -c 'import json,sys; print(json.load(sys.stdin)["terraform_version"])')" == 1.10.9 ]] || exit 2
(cd "$ARTIFACT" && sha256sum --check SHA256SUMS)
WORK="$(mktemp -d)"
trap 'rm -f "$WORK/backend.json" "$WORK/plan" "$WORK/deploy.log"' EXIT
export TF_DATA_DIR="$WORK/tofu"
export AWS_ACCESS_KEY_ID="$LIVE_STATE_ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="$LIVE_STATE_SECRET_ACCESS_KEY"
unset AWS_PROFILE AWS_SESSION_TOKEN AWS_ENDPOINT_URL AWS_ENDPOINT_URL_S3
python3 "$ROOT/scripts/live-state.py" --environment "$ENVIRONMENT" --output "$WORK/backend.json"
export TF_VAR_environment="$ENVIRONMENT"
export TF_VAR_account_id="$CLOUDFLARE_ACCOUNT_ID"
export TF_VAR_zone_id="$CLOUDFLARE_ZONE_ID"
export TF_VAR_hostname="$LIVE_HOSTNAME"
export TF_VAR_pages_project="$LIVE_PAGES_PROJECT"
export TF_VAR_publisher_key_sha256="$LIVE_PUBLISHER_KEY_SHA256"
export TF_VAR_relay_directory="$ARTIFACT/relay"
cd "$ARTIFACT/infrastructure"
# Suppress provider diagnostics, which can echo bindings. Locking is mandatory.
run_private() {
  if ! "$@" > "$WORK/deploy.log" 2>&1; then
    echo 'Deployment operation failed; provider output suppressed. Inspect protected state before retry.' >&2
    exit 1
  fi
}
run_private tofu init -input=false -lockfile=readonly -backend-config="$WORK/backend.json"
run_private tofu plan -input=false -lock-timeout=5m -out="$WORK/plan"
run_private tofu apply -input=false -lock-timeout=5m "$WORK/plan"
# Pages owns only static assets. No Wrangler Worker upload/deploy or DO migrations.
run_private "$ARTIFACT/tools/node_modules/.bin/wrangler" pages deploy "$ARTIFACT/site" \
  --project-name="$LIVE_PAGES_PROJECT" --branch=master --commit-hash="$(cat "$ARTIFACT/REVISION")"
python3 "$ROOT/scripts/smoke-live-broadcast.py" --url "https://$LIVE_HOSTNAME"
echo 'Environment deployed; public read-only health/status/viewer smoke passed.'
