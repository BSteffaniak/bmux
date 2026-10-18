#!/usr/bin/env bash
# Authenticated read-only infrastructure check before compiling any candidates.
# Its synthetic image is never uploaded, approved, or applied.
set -euo pipefail
umask 077
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT="${1:?Temporary output directory required}"
[[ ! -e "$OUTPUT" ]] || { echo 'Output must not exist' >&2; exit 2; }
mkdir -p "$OUTPUT/infrastructure" "$OUTPUT/relay"
cp "$ROOT"/infra/live-terraform/*.tf "$ROOT/infra/live-terraform/.terraform.lock.hcl" "$OUTPUT/infrastructure/"
printf 'registry.fly.io/%s@sha256:%064d\n' "${LIVE_FLY_APP:?}" 0 > "$OUTPUT/relay/IMAGE"
git -C "$ROOT" rev-parse HEAD > "$OUTPUT/REVISION"
(cd "$OUTPUT" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
bash "$ROOT/scripts/live-deploy.sh" production "$OUTPUT" check
