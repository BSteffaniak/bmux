#!/usr/bin/env bash
# Run through `bash scripts/dev.sh bash scripts/build-web-renderer.sh`.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
: "${BMUX_NIX_TOOLCHAIN:?Run inside nix develop or bash scripts/dev.sh}"
: "${CARGO_TARGET_DIR:?Use the Nix-selected target directory}"
BINDGEN="${BMUX_WASM_BINDGEN:-$ROOT/target/web-tools/bin/wasm-bindgen}"
if [[ ! -x "$BINDGEN" ]]; then
  echo "Install the matching binding generator inside the Nix shell:" >&2
  echo "cargo install wasm-bindgen-cli --version 0.2.126 --locked --root $ROOT/target/web-tools" >&2
  exit 1
fi
if [[ "$("$BINDGEN" --version)" != "wasm-bindgen 0.2.126" ]]; then
  echo "wasm-bindgen must match the workspace's 0.2.126 crate version" >&2
  exit 1
fi
cargo build -p bmux_web_renderer --features wasm --target wasm32-unknown-unknown
mkdir -p "$ROOT/target/web-assets"
"$BINDGEN" "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/bmux_web_renderer.wasm" \
  --target web --out-dir "$ROOT/target/web-assets"
cp packages/web-renderer/browser/index.html packages/web-renderer/browser/bootstrap.js \
  packages/web-renderer/browser/style.css target/web-assets/
echo "Browser assets: $ROOT/target/web-assets"
