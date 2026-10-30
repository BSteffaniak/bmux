#!/usr/bin/env bash
# Credentialed CI/operator entry point. No implicit infrastructure adoption.
set -euo pipefail
umask 077
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENVIRONMENT="${1:?Usage: live-deploy.sh production ARTIFACT_DIRECTORY [release|preview|check|configure|approved|inspect|reconcile-mount]}"
[[ "$ENVIRONMENT" == staging || "$ENVIRONMENT" == production ]] || exit 2
ARTIFACT="$(cd "${2:?Artifact directory required}" && pwd)"
MODE="${3:-release}"
[[ "$MODE" == release || "$MODE" == preview || "$MODE" == check || "$MODE" == configure || "$MODE" == approved || "$MODE" == inspect || "$MODE" == reconcile-mount ]] || exit 2
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
: "${LIVE_METRICS_KEY_SHA256:?Separate metrics digest required}"
: "${FLY_API_TOKEN:?Fly deployment credential required}"
: "${LIVE_FLY_APP:?}"
: "${LIVE_FLY_ORG:?}"
: "${LIVE_PAGES_PROJECT:?Selected Pages project required}"
: "${R2_STATE_ACCESS_KEY_ID:?Private R2 state access key required}"
: "${R2_STATE_SECRET_ACCESS_KEY:?Private R2 state secret required}"
: "${TOFU_STATE_ENCRYPTION_PASSPHRASE:?Private client-side encryption passphrase required}"
[[ -z "${LIVE_STATE_ROLE_ARN:-}${LIVE_STATE_ACCESS_KEY_ID:-}${LIVE_STATE_SECRET_ACCESS_KEY:-}" ]] || { echo 'Legacy AWS state configuration rejected; manual migration required' >&2; exit 2; }
# Clear ambient AWS/Tofu configuration before binding the single R2 authority.
ambient_config_names="$(python3 -c 'import os; print("\n".join(k for k in os.environ if k.startswith(("AWS_", "TF_", "TOFU_"))))')"
while IFS= read -r name; do
  case "$name" in AWS_*|TF_*|TOFU_*) [[ "$name" == TOFU_STATE_ENCRYPTION_PASSPHRASE ]] || unset "$name" ;; esac
done <<< "$ambient_config_names"
unset ambient_config_names
export AWS_ACCESS_KEY_ID="$R2_STATE_ACCESS_KEY_ID" AWS_SECRET_ACCESS_KEY="$R2_STATE_SECRET_ACCESS_KEY"
export AWS_REGION=auto AWS_DEFAULT_REGION=auto AWS_EC2_METADATA_DISABLED=true
export AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null AWS_PAGER=''
export TF_VAR_state_bucket_name="${LIVE_STATE_BUCKET:?}"
export TF_VAR_state_encryption_passphrase="$TOFU_STATE_ENCRYPTION_PASSPHRASE"
export LIVE_ENVIRONMENT="$ENVIRONMENT" LIVE_STATE_REGION="${LIVE_STATE_REGION:-auto}"
[[ "$(tofu version -json | python3 -c 'import json,sys; print(json.load(sys.stdin)["terraform_version"])')" == 1.10.9 ]] || exit 2
(cd "$ARTIFACT" && sha256sum --check SHA256SUMS)
LIVE_RELAY_IMAGE="$(cat "$ARTIFACT/relay/IMAGE")"
export LIVE_RELAY_IMAGE
python3 - <<'PY'
import os, re
app = os.environ['LIVE_FLY_APP']
image = os.environ['LIVE_RELAY_IMAGE']
if not re.fullmatch('[a-z0-9][a-z0-9-]{0,62}', app) or not re.fullmatch(re.escape('registry.fly.io/' + app) + r'@sha256:[a-f0-9]{64}', image):
    raise SystemExit('Invalid candidate image identity')
PY
WORK="$(mktemp -d)"
chmod 700 "$WORK"
IMPORT_FILE="$ARTIFACT/infrastructure/approved-imports.tf.json"
PRESERVE_FILE="$ARTIFACT/infrastructure/migration-retention_override.tf.json"
[[ ! -e "$PRESERVE_FILE" && ! -L "$PRESERVE_FILE" ]] || { echo 'Unexpected migration override in immutable candidate' >&2; exit 2; }
[[ ! -e "$IMPORT_FILE" && ! -L "$IMPORT_FILE" ]] || { echo 'Unexpected import file in immutable candidate' >&2; exit 2; }
trap 'rm -f "$WORK/backend.json" "$WORK/plan" "$WORK/plan.json" "$WORK/managed.json" "$WORK/imports.json" "$WORK/state-check.json" "$WORK/archive-before.json" "$WORK/archive-after.json" "$WORK/deploy.log" "$WORK/inspect-state.json" "$WORK/inspect-plan.json" "$WORK/state-before.json" "$WORK/state-candidate.json" "$WORK/state-verify.json" "$WORK/registry-auth.json" "$IMPORT_FILE" "$PRESERVE_FILE"' EXIT
export TF_DATA_DIR="$WORK/tofu"
python3 "$ROOT/scripts/live-state.py" --environment "$ENVIRONMENT" --output "$WORK/backend.json"
export TF_VAR_environment="$ENVIRONMENT" TF_VAR_account_id="$CLOUDFLARE_ACCOUNT_ID"
export TF_VAR_zone_id="$CLOUDFLARE_ZONE_ID" TF_VAR_hostname="$LIVE_HOSTNAME"
export TF_VAR_pages_project="$LIVE_PAGES_PROJECT" TF_VAR_publisher_key_sha256="$LIVE_PUBLISHER_KEY_SHA256"
export TF_VAR_metrics_key_sha256="$LIVE_METRICS_KEY_SHA256"
export TF_VAR_fly_app_name="$LIVE_FLY_APP" TF_VAR_fly_org="$LIVE_FLY_ORG"
export TF_VAR_fly_region="${LIVE_FLY_REGION:-iad}" TF_VAR_relay_hostname="${LIVE_RELAY_HOSTNAME:-live.bmux.dev}"
export TF_VAR_relay_image="$LIVE_RELAY_IMAGE"
cd "$ARTIFACT/infrastructure"
run_private() {
  if ! "$@" > "$WORK/deploy.log" 2>&1; then
    echo "Deployment operation failed: $1 ${2:-}. Sanitized diagnostics:" >&2
    python3 "$ROOT/scripts/live-deploy-diagnostics.py" "$WORK/deploy.log"
    return 1
  fi
}
ADMISSION_ARGS=()
if [[ -n "$BOOTSTRAP_COMPLETION" ]]; then
  python3 "$ROOT/scripts/archive-live-state.py" admit --bootstrap-completion "$BOOTSTRAP_COMPLETION" --admission-file "$WORK/admission.json" > "$WORK/state-check.json"
  ADMISSION_ARGS=(--admission-file "$WORK/admission.json")
fi
python3 "$ROOT/scripts/archive-live-state.py" check "${ADMISSION_ARGS[@]}" > "$WORK/state-check.json"
run_private tofu init -json -input=false -lockfile=readonly -backend-config="$WORK/backend.json"
if [[ "$MODE" == reconcile-mount ]]; then
  # Explicit incident recovery only. No plan/apply/import/image upload. Keep
  # OpenTofu encryption, lineage, serial checks and backend locking intact.
  python3 "$ROOT/scripts/archive-live-state.py" release-check > "$WORK/state-check.json"
  python3 "$ROOT/scripts/archive-live-state.py" archive > "$WORK/archive-before.json"
  run_private tofu state pull
  mv "$WORK/deploy.log" "$WORK/state-before.json"
  python3 "$ROOT/scripts/reconcile-live-mounts.py" "$WORK/state-before.json" "$WORK/state-candidate.json"
  run_private tofu state push -lock-timeout=5m "$WORK/state-candidate.json"
  # Archive immediately after the write, including if subsequent verification
  # fails. Never auto-restore over a concurrent or ambiguously completed write.
  python3 "$ROOT/scripts/archive-live-state.py" archive > "$WORK/archive-after.json"
  run_private tofu state pull
  mv "$WORK/deploy.log" "$WORK/state-verify.json"
  python3 "$ROOT/scripts/reconcile-live-mounts.py" --verify "$WORK/state-candidate.json" "$WORK/state-verify.json"
  echo 'Incident ownership reconciled in encrypted state only; run a fresh release preview.'
  exit 0
fi
if [[ "$MODE" == inspect ]]; then
  # No state writes, lock acquisition, imports, image uploads or apply. Plan
  # refresh is transient; show reads persisted ownership separately for comparison.
  run_private tofu show -json
  mv "$WORK/deploy.log" "$WORK/inspect-state.json"
  run_private tofu plan -json -input=false -lock=false -out="$WORK/plan"
  run_private tofu show -json "$WORK/plan"
  mv "$WORK/deploy.log" "$WORK/inspect-plan.json"
  python3 "$ROOT/scripts/inspect-live-mounts.py" "$WORK/inspect-state.json" "$WORK/inspect-plan.json"
  exit 0
fi
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
if [[ "$MODE" == configure ]]; then
  python3 "$ROOT/scripts/live-fly-preflight.py" "$WORK/managed.json"
fi
if [[ "$MODE" != configure || -z "$BOOTSTRAP_COMPLETION" ]]; then
  python3 "$ROOT/scripts/archive-live-state.py" release-check > "$WORK/state-check.json"
  if [[ "$MODE" == check ]]; then
    echo 'Canonical encrypted state and cutover fence checked (read-only).'
    exit 0
  fi
fi
if [[ "$MODE" != preview && "$MODE" != approved ]]; then
  python3 "$ROOT/scripts/archive-live-state.py" archive "${ADMISSION_ARGS[@]}" > "$WORK/archive-before.json"
fi
POLICY_ARGS=()
[[ "$MODE" != configure ]] || POLICY_ARGS+=(--configure)
if [[ "$MODE" == preview || "$MODE" == approved ]]; then
  POLICY_ARGS+=(--infrastructure)
  if [[ "$MODE" == preview ]]; then POLICY_ARGS+=(--review preview); else POLICY_ARGS+=(--review apply); fi
fi
# Publish the inventory-verified prebuilt OCI image only after policy accepts
# the exact saved plan and, in approved mode, matches its reviewed receipt. Credentials remain in a private, transient auth file.
# skopeo --preserve-digests rejects any registry rewrite of image identity.
apply_plan() {
  run_private tofu plan -json -input=false -lock-timeout=5m "$@" -out="$WORK/plan"
  if ! tofu show -json "$WORK/plan" > "$WORK/plan.json" 2> "$WORK/deploy.log"; then
    echo 'Plan inspection failed; provider output suppressed' >&2; exit 1
  fi
  python3 "$ROOT/scripts/live-plan-policy.py" "$WORK/plan.json" "${POLICY_ARGS[@]}"
  if [[ "$MODE" == preview ]]; then
    echo 'Release plan accepted: known non-destructive infrastructure changes and prebuilt image reviewed.'
    exit 0
  fi
  if [[ "$MODE" == approved ]]; then
    # No production writes, including archive/image upload, before receipt check.
    python3 "$ROOT/scripts/archive-live-state.py" archive > "$WORK/archive-before.json"
  fi
  if [[ -n "$BOOTSTRAP_COMPLETION" ]]; then
    python3 "$ROOT/scripts/archive-live-state.py" begin "${ADMISSION_ARGS[@]}" > "$WORK/state-check.json"
    BOOTSTRAP_COMPLETION=''
  fi
  if [[ "${1:-}" != -target=fly_app.live ]]; then
    printf '%s' "$FLY_API_TOKEN" | run_private skopeo login --authfile "$WORK/registry-auth.json" \
      --username x --password-stdin registry.fly.io
    run_private skopeo copy --authfile "$WORK/registry-auth.json" --preserve-digests \
      "oci-archive:$ARTIFACT/relay/image.oci.tar" "docker://$LIVE_RELAY_IMAGE"
    rm -f "$WORK/registry-auth.json"
  fi
  run_private tofu apply -json -input=false -lock-timeout=5m "$WORK/plan"
  python3 "$ROOT/scripts/archive-live-state.py" archive > "$WORK/archive-after.json"
}
if [[ "$MODE" == configure ]]; then
  # Fly registry authorization requires the app to exist before image upload.
  # Archive this genuine declarative resource stage before proceeding.
  apply_plan -target=fly_app.live
fi
apply_plan
# Pages has a separate publisher; backend releases never upload static assets.
python3 "$ROOT/scripts/smoke-live-broadcast.py" --url "https://$LIVE_HOSTNAME" --relay-url "https://${LIVE_RELAY_HOSTNAME:-live.bmux.dev}" --component backend
echo 'Production candidate deployed; public read-only smoke passed.'
