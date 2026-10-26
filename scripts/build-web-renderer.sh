#!/usr/bin/env bash
# Explicit web setup only; native product builds must never invoke this script.
# Run through `nix develop --command bash scripts/build-web-renderer.sh`.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
: "${BMUX_NIX_TOOLCHAIN:?Run inside nix develop or a direnv-loaded shell}"
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
# Browser consumers must not link the native Wasmtime host runtime. Build-time
# dependencies are deliberately excluded: they execute on the build host.
BROWSER_DEPS="$(cargo tree --locked -p bmux_presenter --target wasm32-unknown-unknown --edges normal --prefix none)"
if grep -Eq '^wasmtime v' <<< "$BROWSER_DEPS"; then
    echo "error: browser presenter depends on the native Wasmtime runtime" >&2
    exit 1
fi
cargo build --locked -p bmux_web_renderer --features wasm --target wasm32-unknown-unknown --release
cargo build --locked -p bmux_presenter --features wasm --target wasm32-unknown-unknown --release
"$BINDGEN" "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/bmux_presenter.wasm" \
  --target web --out-dir "$OUTPUT" --out-name bmux_presenter
cp scripts/presenter-adapter.js "$OUTPUT/"
"$BINDGEN" "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/bmux_web_renderer.wasm" \
  --target web --out-dir "$OUTPUT" --out-name bmux_web_renderer
cp packages/web-renderer/browser/index.html packages/web-renderer/browser/bootstrap.js \
  packages/web-renderer/browser/style.css packages/web-renderer/browser/canvas-viewer.js \
  packages/web-renderer/browser/live-viewer.js packages/web-renderer/browser/live-page.js "$OUTPUT/"
echo "Browser assets: $OUTPUT"
# Bind the public relay origin into the immutable module before hashing it.
python3 - "$OUTPUT/live-page.js" "${BMUX_LIVE_ENDPOINT_ORIGIN:-https://live.bmux.dev}" <<'PY'
import json
from pathlib import Path
import re
import sys
from urllib.parse import urlsplit
origin = sys.argv[2]
parsed = urlsplit(origin)
if (parsed.scheme != 'https' or not parsed.hostname or parsed.username is not None
        or parsed.password is not None or parsed.path not in ('', '/')
        or parsed.query or parsed.fragment):
    raise SystemExit('BMUX_LIVE_ENDPOINT_ORIGIN must be an HTTPS origin without credentials')
origin = origin.rstrip('/') + '/'
page = Path(sys.argv[1])
text, count = re.subn(r"apiBase: (?:root\.dataset\?\.liveRelay \|\| )?(?:'https://live\.bmux\.dev/'|'/api/live/main/')",
                      lambda _: 'apiBase: ' + json.dumps(origin), page.read_text())
if count != 1:
    raise SystemExit('public live module relay-origin binding is missing or ambiguous')
page.write_text(text)
PY
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
    "renderer": "bmwp-5", "bundle_sha256": sys.argv[2], "files": files}, indent=2) + "\n")
PY
printf '%s\n' "$VERSION" > "$DOCS_ASSETS/current-version"
echo "Docs audience bundle: $DOCS_ASSETS/$VERSION"
