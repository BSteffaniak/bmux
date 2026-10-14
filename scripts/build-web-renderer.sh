#!/usr/bin/env bash
# Explicit web setup only; native product builds must never invoke this script.
# Run through `bash scripts/dev.sh bash scripts/build-web-renderer.sh`.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
: "${BMUX_NIX_TOOLCHAIN:?Run inside nix develop or bash scripts/dev.sh}"
: "${CARGO_TARGET_DIR:?Use the Nix-selected target directory}"
OUTPUT="${BMUX_WEB_ASSET_OUTPUT:-$ROOT/target/web-assets}"
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
TOOLS="${BMUX_WEB_TOOLS_ROOT:-$ROOT/target/web-tools}"
BINDGEN="${BMUX_WASM_BINDGEN:-$TOOLS/bin/wasm-bindgen}"
if [[ ! -x "$BINDGEN" ]]; then
  if [[ -n "${BMUX_WASM_BINDGEN:-}" ]]; then
    echo "Explicit BMUX_WASM_BINDGEN is not executable: $BINDGEN" >&2
    exit 1
  fi
  cargo install wasm-bindgen-cli --version 0.2.126 --locked --root "$TOOLS"
fi
if [[ "$("$BINDGEN" --version)" != "wasm-bindgen 0.2.126" ]]; then
  echo "wasm-bindgen must match the workspace's 0.2.126 crate version" >&2
  exit 1
fi
cargo build --locked -p bmux_web_renderer --features wasm --target wasm32-unknown-unknown --release
"$BINDGEN" "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/bmux_web_renderer.wasm" \
  --target web --out-dir "$OUTPUT" --out-name bmux_web_renderer
cp packages/web-renderer/browser/index.html packages/web-renderer/browser/bootstrap.js \
  packages/web-renderer/browser/style.css packages/web-renderer/browser/canvas-viewer.js \
  packages/web-renderer/browser/live-viewer.js packages/web-renderer/browser/live-page.js "$OUTPUT/"
echo "Browser assets: $OUTPUT"
# Stage immutable audience assets before docs compilation/static generation.
# Native builds never invoke this script or depend on these files.
DOCS_ASSETS="$ROOT/packages/docs-site/public/live-assets"
ASSET_FILES=(canvas-viewer.js live-viewer.js live-page.js bmux_web_renderer.js bmux_web_renderer_bg.wasm)
VERSION="$(for name in "${ASSET_FILES[@]}"; do sha256sum "$OUTPUT/$name" | cut -d ' ' -f 1; done | sha256sum | cut -d ' ' -f 1)"
mkdir -p "$DOCS_ASSETS/$VERSION"
for name in "${ASSET_FILES[@]}"; do cp "$OUTPUT/$name" "$DOCS_ASSETS/$VERSION/$name"; done
python3 - "$DOCS_ASSETS/$VERSION" "$VERSION" "${ASSET_FILES[@]}" <<'PY'
import hashlib
import json
from pathlib import Path
import sys
root = Path(sys.argv[1])
files = []
for name in sys.argv[3:]:
    data = (root / name).read_bytes()
    files.append({"name": name, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()})
(root / "manifest.json").write_text(json.dumps({"schema": 1, "protocol": 1,
    "renderer": "bmwp-2", "bundle_sha256": sys.argv[2], "files": files}, indent=2) + "\n")
PY
printf '%s\n' "$VERSION" > "$DOCS_ASSETS/current-version"
echo "Docs audience bundle: $DOCS_ASSETS/$VERSION"
