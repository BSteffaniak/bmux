#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

manifest="packages/tui-components/Cargo.toml"

mapfile -t features < <(
  python3 - "$manifest" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as file:
    manifest = tomllib.load(file)

excluded = {"default", "fail-on-warnings", "keyboard-input"}
for feature in manifest["features"]:
    if feature not in excluded:
        print(feature)
PY
)

cargo check -p bmux_tui_components --no-default-features
for feature in "${features[@]}"; do
  echo "Checking bmux_tui_components feature: $feature"
  cargo check -p bmux_tui_components --no-default-features --features "$feature"
done

# Keep this explicit even though `all` is included above: it verifies Cargo's
# complete feature closure rather than only the crate's convenience bundle.
cargo check -p bmux_tui_components --all-features

echo "bmux_tui_components feature matrix passed"
