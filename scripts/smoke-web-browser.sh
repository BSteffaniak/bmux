#!/usr/bin/env bash
# Source-only harness: never builds assets/plugins/binaries or installs packages.
# Run only after acquiring coordinator browser/isolated-runtime resources.
# Example (inside the locked shell, with prebuilt artifacts):
# bash scripts/smoke-web-browser.sh --assets "$PWD/target/web-assets" \
#   --browser /absolute/path/to/chromium --standalone-snapshot /tmp/example.json
# --binary overrides metadata discovery; otherwise nix develop --command cargo metadata
# supplies the locked toolchain target directory, never a guessed Nix path.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v python3 >/dev/null || { echo 'FAIL: python3 unavailable' >&2; exit 1; }
args=("$@")
has_binary=0
for arg in "${args[@]}"; do
  case "$arg" in
    --binary|--binary=*) has_binary=1 ;;
    --help|-h) exec python3 "$ROOT/scripts/web-browser-acceptance.py" --help ;;
  esac
done
if [[ "$has_binary" == 0 ]]; then
  target="$(cd "$ROOT" && nix develop --command cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
  args=(--binary "$target/debug/bmux" "${args[@]}")
fi
exec python3 "$ROOT/scripts/web-browser-acceptance.py" "${args[@]}"
