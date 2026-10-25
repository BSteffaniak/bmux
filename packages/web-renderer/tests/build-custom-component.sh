#!/usr/bin/env bash
# Run through bash scripts/dev.sh; installs nothing unless explicitly requested.
set -euo pipefail
: "${BMUX_NIX_TOOLCHAIN:?Use bash scripts/dev.sh}"
: "${CARGO_TARGET_DIR:?Use the Nix-selected target directory}"
BINDGEN="${BMUX_WASM_BINDGEN:-target/web-tools/bin/wasm-bindgen}"
if [[ ! -x "$BINDGEN" ]] || [[ "$("$BINDGEN" --version)" != "wasm-bindgen 0.2.126" ]]; then
  echo 'Provide wasm-bindgen-cli 0.2.126 via BMUX_WASM_BINDGEN or target/web-tools/bin/wasm-bindgen' >&2
  exit 1
fi
cargo build --locked -p bmux_web_renderer --example custom_component --features wasm --target wasm32-unknown-unknown
"$BINDGEN" --target web --out-dir target/custom-component-web/pkg \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/examples/custom_component.wasm"
