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
  packages/web-renderer/browser/style.css "$OUTPUT/"
echo "Browser assets: $OUTPUT"
