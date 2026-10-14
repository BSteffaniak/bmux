#!/usr/bin/env bash
# Build/package only. Validation runs in CI and Live Validation, not deployment.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
: "${BMUX_NIX_TOOLCHAIN:?Use bash scripts/dev.sh bash scripts/build-live-artifact.sh OUTPUT}"
: "${CARGO_TARGET_DIR:?}"
OUTPUT="${1:?New output directory required}"
[[ ! -e "$OUTPUT" ]] || { echo 'Output already exists' >&2; exit 2; }
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
cargo build --locked --release -p bmux_cli
# Rebuild plugin bundles through the explicitly built test version, isolated state.
ISOLATED="$(mktemp -d "${TMPDIR:-/tmp}/bmux-live-build.XXXXXXXX")"
mkdir -p "$ISOLATED"/{home,config,runtime,data,state}
HOME="$ISOLATED/home" XDG_CONFIG_HOME="$ISOLATED/config" XDG_RUNTIME_DIR="$ISOLATED/runtime" \
  XDG_DATA_HOME="$ISOLATED/data" XDG_STATE_HOME="$ISOLATED/state" \
  BMUX_CONFIG_DIR="$ISOLATED/config" BMUX_RUNTIME_DIR="$ISOLATED/runtime" \
  BMUX_DATA_DIR="$ISOLATED/data" BMUX_STATE_DIR="$ISOLATED/state" BMUX_NO_BASE_CONFIG=1 \
  "$CARGO_TARGET_DIR/release/bmux" plugin rebuild --all-workspace-plugins
BMUX_WEB_ASSET_OUTPUT="$OUTPUT/viewer" bash scripts/build-web-renderer.sh
for asset in canvas-viewer.js live-viewer.js live-page.js; do
  test -s "$OUTPUT/viewer/$asset"
done
# build-web-renderer.sh stages only the SHA-256 named immutable bundle in docs
# public assets BEFORE docs compilation. Do not add an unversioned second copy.
test -s packages/docs-site/public/live-assets/current-version
VERSION="$(cat packages/docs-site/public/live-assets/current-version)"
test -d "packages/docs-site/public/live-assets/$VERSION"
cargo run --locked --release -p bmux_docs_site -- gen --output "$OUTPUT/site"
mkdir "$OUTPUT/native" "$OUTPUT/relay" "$OUTPUT/tools"
cp "$CARGO_TARGET_DIR/release/bmux" "$OUTPUT/native/bmux"
cp -R infra/live-relay/src/. "$OUTPUT/relay/"
cp infra/live-terraform/tools/package*.json "$OUTPUT/tools/"
(cd "$OUTPUT/tools" && npm ci --ignore-scripts --no-audit --no-fund)
git rev-parse HEAD > "$OUTPUT/REVISION"
# Lock and infrastructure are part of the same immutable candidate as code/assets.
mkdir "$OUTPUT/infrastructure"
cp infra/live-terraform/*.tf infra/live-terraform/.terraform.lock.hcl "$OUTPUT/infrastructure/"
# Normalize npm-created executable links to ordinary inventory-covered files.
python3 - "$OUTPUT/tools" <<'PY'
from pathlib import Path
import sys
root = Path(sys.argv[1]).resolve()
for path in root.rglob('*'):
    if path.is_symlink():
        target = path.resolve(strict=True)
        if not target.is_relative_to(root) or not target.is_file():
            raise SystemExit('Unexpected external npm asset symlink')
        payload = target.read_bytes()
        path.unlink()
        path.write_bytes(payload)
PY
# The inventory file is explicitly excluded from find's input set.
# shellcheck disable=SC2094
(cd "$OUTPUT" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
echo "Immutable live candidate: $OUTPUT"
