#!/usr/bin/env bash
# Credentialed CI/operator entry point. No implicit infrastructure adoption.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENVIRONMENT="${1:?Usage: live-deploy.sh production ARTIFACT_DIRECTORY [configure]}"
[[ "$ENVIRONMENT" == staging || "$ENVIRONMENT" == production ]] || exit 2
ARTIFACT="$(cd "${2:?Artifact directory required}" && pwd)"
MODE="${3:-release}"
[[ "$MODE" == release || "$MODE" == configure ]] || exit 2
BOOTSTRAP_COMPLETION=''
if [[ $# -gt 3 ]]; then
  [[ $# == 5 && "$MODE" == configure && "$4" == --bootstrap-completion ]] || { echo 'Bootstrap completion is only supported for explicit first configure' >&2; exit 2; }
  BOOTSTRAP_COMPLETION="$5"
fi
: "${CLOUDFLARE_API_TOKEN:?Scoped deployment credential required}"
: "${CLOUDFLARE_ACCOUNT_ID:?}"
: "${CLOUDFLARE_ZONE_ID:?}"
: "${LIVE_HOSTNAME:?}"
: "${LIVE_PUBLISHER_KEY_SHA256:?Digest only; never publisher key}"
: "${LIVE_PAGES_PROJECT:?Selected Pages project required}"
: "${R2_STATE_ACCESS_KEY_ID:?Private R2 state access key required}"
: "${R2_STATE_SECRET_ACCESS_KEY:?Private R2 state secret required}"
: "${TOFU_STATE_ENCRYPTION_PASSPHRASE:?Private client-side encryption passphrase required}"
[[ -z "${LIVE_STATE_ROLE_ARN:-}${LIVE_STATE_ACCESS_KEY_ID:-}${LIVE_STATE_SECRET_ACCESS_KEY:-}" ]] || { echo 'Legacy AWS state configuration rejected; manual migration required' >&2; exit 2; }
# Clear ambient AWS/Tofu configuration before binding the single R2 authority.
for name in $(compgen -e); do
  case "$name" in AWS_*|TF_*|TOFU_*) [[ "$name" == TOFU_STATE_ENCRYPTION_PASSPHRASE ]] || unset "$name" ;; esac
done
export AWS_ACCESS_KEY_ID="$R2_STATE_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_STATE_SECRET_ACCESS_KEY"
export AWS_REGION=auto AWS_DEFAULT_REGION=auto AWS_EC2_METADATA_DISABLED=true
export AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null AWS_PAGER=''
export TF_VAR_state_bucket_name="${LIVE_STATE_BUCKET:?}"
export TF_VAR_state_encryption_passphrase="$TOFU_STATE_ENCRYPTION_PASSPHRASE"
export LIVE_ENVIRONMENT="$ENVIRONMENT" LIVE_STATE_REGION="${LIVE_STATE_REGION:-auto}"
[[ "$(tofu version -json | python3 -c 'import json,sys; print(json.load(sys.stdin)["terraform_version"])')" == 1.10.9 ]] || exit 2
(cd "$ARTIFACT" && sha256sum --check SHA256SUMS)
WORK="$(mktemp -d)"
chmod 700 "$WORK"
IMPORT_FILE="$ARTIFACT/infrastructure/approved-imports.tf.json"
[[ ! -e "$IMPORT_FILE" && ! -L "$IMPORT_FILE" ]] || { echo 'Unexpected import file in immutable candidate' >&2; exit 2; }
trap 'rm -f "$WORK/backend.json" "$WORK/plan" "$WORK/plan.json" "$WORK/managed.json" "$WORK/imports.json" "$WORK/state-check.json" "$WORK/archive-before.json" "$WORK/archive-after.json" "$WORK/deploy.log" "$IMPORT_FILE"' EXIT
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
ADMISSION_ARGS=()
if [[ -n "$BOOTSTRAP_COMPLETION" ]]; then
  python3 "$ROOT/scripts/archive-live-state.py" admit --bootstrap-completion "$BOOTSTRAP_COMPLETION" --admission-file "$WORK/admission.json" > "$WORK/state-check.json"
  ADMISSION_ARGS=(--admission-file "$WORK/admission.json")
fi
python3 "$ROOT/scripts/archive-live-state.py" check "${ADMISSION_ARGS[@]}" > "$WORK/state-check.json"
run_private tofu init -input=false -lockfile=readonly -backend-config="$WORK/backend.json"
if [[ "$MODE" == configure ]]; then
  if [[ -n "$BOOTSTRAP_COMPLETION" ]]; then
    # Only the verified admission path may have no state. Recheck the remote
    # marker-only inventory after init; do not swallow arbitrary state-list errors.
    python3 "$ROOT/scripts/archive-live-state.py" check "${ADMISSION_ARGS[@]}" > "$WORK/state-check.json"
    printf '[]' > "$WORK/managed.json"
  else
    run_private tofu state list
    python3 -c 'import json,sys; json.dump(sys.stdin.read().splitlines(),sys.stdout)' < "$WORK/deploy.log" > "$WORK/managed.json"
  fi
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
python3 "$ROOT/scripts/archive-live-state.py" archive "${ADMISSION_ARGS[@]}" > "$WORK/archive-before.json"
POLICY_ARGS=()
[[ "$MODE" != configure ]] || POLICY_ARGS+=(--configure)
MIGRATION_ARGS=()
if [[ "$MODE" == configure ]]; then
  MIGRATION_NEEDED="$(python3 -c 'import json,sys; managed=json.load(open(sys.argv[1])); assert isinstance(managed,list) and all(isinstance(x,str) for x in managed); print(str("cloudflare_workers_deployment.live" not in managed).lower())' "$WORK/managed.json")"
  if [[ "$MIGRATION_NEEDED" == true ]]; then
    MIGRATION_ARGS=(-var=live_migration_only=true)
  fi
fi
apply_plan() {
  run_private tofu plan -input=false -lock-timeout=5m "$@" -out="$WORK/plan"
  if ! tofu show -json "$WORK/plan" > "$WORK/plan.json" 2> "$WORK/deploy.log"; then
    echo 'Plan inspection failed; provider output suppressed' >&2; exit 1
  fi
  local stage_policy=()
  [[ "${1:-}" != -var=live_migration_only=true ]] || stage_policy=(--migration)
  python3 "$ROOT/scripts/live-plan-policy.py" "$WORK/plan.json" "${POLICY_ARGS[@]}" "${stage_policy[@]}"
  if [[ -n "$BOOTSTRAP_COMPLETION" ]]; then
    python3 "$ROOT/scripts/archive-live-state.py" begin "${ADMISSION_ARGS[@]}" > "$WORK/state-check.json"
    BOOTSTRAP_COMPLETION=''
  fi
  run_private tofu apply -input=false -lock-timeout=5m "$WORK/plan"
  python3 "$ROOT/scripts/archive-live-state.py" archive > "$WORK/archive-after.json"
}
if [[ ${#MIGRATION_ARGS[@]} -gt 0 ]]; then
  # Target excludes the route. The migration must be deployed before a runtime
  # version may bind to its class (Cloudflare error 100123).
  apply_plan "${MIGRATION_ARGS[@]}" -target=cloudflare_workers_deployment.live
fi
apply_plan
# Pages has a separate publisher; backend releases never upload static assets.
python3 "$ROOT/scripts/smoke-live-broadcast.py" --url "https://$LIVE_HOSTNAME" --component backend
echo 'Production candidate deployed; public read-only smoke passed.'
