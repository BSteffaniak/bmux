#!/usr/bin/env bash
# Credentialed CI/operator entry point. No implicit infrastructure adoption.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENVIRONMENT="${1:?Usage: live-deploy.sh production ARTIFACT_DIRECTORY [configure]}"
[[ "$ENVIRONMENT" == staging || "$ENVIRONMENT" == production ]] || exit 2
ARTIFACT="$(cd "${2:?Artifact directory required}" && pwd)"
MODE="${3:-release}"
[[ "$MODE" == release || "$MODE" == configure ]] || exit 2
: "${CLOUDFLARE_API_TOKEN:?Scoped deployment credential required}"
: "${CLOUDFLARE_ACCOUNT_ID:?}"
: "${CLOUDFLARE_ZONE_ID:?}"
: "${LIVE_HOSTNAME:?}"
: "${LIVE_PUBLISHER_KEY_SHA256:?Digest only; never publisher key}"
: "${LIVE_PAGES_PROJECT:?Selected Pages project required}"
if [[ -n "${LIVE_STATE_ACCESS_KEY_ID:-}" || -n "${LIVE_STATE_SECRET_ACCESS_KEY:-}" ]]; then
  : "${LIVE_STATE_ACCESS_KEY_ID:?Both legacy state credentials required}"
  : "${LIVE_STATE_SECRET_ACCESS_KEY:?Both legacy state credentials required}"
  [[ -z "${LIVE_STATE_ROLE_ARN:-}" ]] || { echo 'Choose OIDC or legacy state credentials, not both' >&2; exit 2; }
  export AWS_ACCESS_KEY_ID="$LIVE_STATE_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$LIVE_STATE_SECRET_ACCESS_KEY"
  unset AWS_SESSION_TOKEN
else
  : "${LIVE_STATE_ROLE_ARN:?Configure the production state OIDC role}"
  : "${AWS_ACCESS_KEY_ID:?CI must assume the configured state role first}"
  : "${AWS_SECRET_ACCESS_KEY:?}"
  : "${AWS_SESSION_TOKEN:?Short-lived OIDC session required}"
fi
unset AWS_PROFILE AWS_ENDPOINT_URL AWS_ENDPOINT_URL_S3
[[ "$(tofu version -json | python3 -c 'import json,sys; print(json.load(sys.stdin)["terraform_version"])')" == 1.10.9 ]] || exit 2
(cd "$ARTIFACT" && sha256sum --check SHA256SUMS)
WORK="$(mktemp -d)"
chmod 700 "$WORK"
IMPORT_FILE="$ARTIFACT/infrastructure/approved-imports.tf.json"
[[ ! -e "$IMPORT_FILE" && ! -L "$IMPORT_FILE" ]] || { echo 'Unexpected import file in immutable candidate' >&2; exit 2; }
trap 'rm -f "$WORK/backend.json" "$WORK/plan" "$WORK/plan.json" "$WORK/managed.json" "$WORK/imports.json" "$WORK/deploy.log" "$IMPORT_FILE"' EXIT
export TF_DATA_DIR="$WORK/tofu"
python3 "$ROOT/scripts/live-state.py" --environment "$ENVIRONMENT" --output "$WORK/backend.json"
export TF_VAR_environment="$ENVIRONMENT" TF_VAR_account_id="$CLOUDFLARE_ACCOUNT_ID"
export TF_VAR_zone_id="$CLOUDFLARE_ZONE_ID" TF_VAR_hostname="$LIVE_HOSTNAME"
export TF_VAR_pages_project="$LIVE_PAGES_PROJECT" TF_VAR_publisher_key_sha256="$LIVE_PUBLISHER_KEY_SHA256"
export TF_VAR_relay_directory="$ARTIFACT/relay"
cd "$ARTIFACT/infrastructure"
run_private() {
  if ! "$@" > "$WORK/deploy.log" 2>&1; then
    echo 'Deployment operation failed; provider output suppressed. Inspect protected state before retry.' >&2
    exit 1
  fi
}
run_private tofu init -input=false -lockfile=readonly -backend-config="$WORK/backend.json"
if [[ "$MODE" == configure ]]; then
  run_private tofu state list
  python3 -c 'import json,sys; json.dump(sys.stdin.read().splitlines(),sys.stdout)' < "$WORK/deploy.log" > "$WORK/managed.json"
  # Exact imports require an independently reviewed resource map; no blind adoption.
  REVIEWED_IMPORTS="${LIVE_REVIEWED_IMPORTS:-}"
  [[ -n "$REVIEWED_IMPORTS" ]] || REVIEWED_IMPORTS='{}'
  printf '%s' "$REVIEWED_IMPORTS" > "$WORK/imports.json"
  IMPORT_ARGS=()
  [[ "${LIVE_APPROVE_IMPORT:-false}" != true ]] || IMPORT_ARGS+=(--approve-import)
  chmod 700 "$ARTIFACT/infrastructure"
  export LIVE_ENVIRONMENT="$ENVIRONMENT"
  python3 "$ROOT/scripts/configure-live-imports.py" --artifact "$ARTIFACT/infrastructure" \
    --output "$ARTIFACT/infrastructure/approved-imports.tf.json" --managed "$WORK/managed.json" \
    --imports "$WORK/imports.json" "${IMPORT_ARGS[@]}"
fi
run_private tofu plan -input=false -lock-timeout=5m -out="$WORK/plan"
if ! tofu show -json "$WORK/plan" > "$WORK/plan.json" 2> "$WORK/deploy.log"; then
  echo 'Plan inspection failed; provider output suppressed' >&2; exit 1
fi
POLICY_ARGS=()
[[ "$MODE" != configure ]] || POLICY_ARGS+=(--configure)
python3 "$ROOT/scripts/live-plan-policy.py" "$WORK/plan.json" "${POLICY_ARGS[@]}"
run_private tofu apply -input=false -lock-timeout=5m "$WORK/plan"
# Pages uploads use the same immutable candidate; Worker deployment remains OpenTofu-owned.
run_private node "$ARTIFACT/tools/node_modules/wrangler/bin/wrangler.js" pages deploy "$ARTIFACT/site" \
  --project-name="$LIVE_PAGES_PROJECT" --branch=master --commit-hash="$(cat "$ARTIFACT/REVISION")"
python3 "$ROOT/scripts/smoke-live-broadcast.py" --url "https://$LIVE_HOSTNAME"
echo 'Production candidate deployed; public read-only smoke passed.'
