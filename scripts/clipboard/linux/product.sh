#!/usr/bin/env bash
# From the source checkout to build (use the same script against each revision).
set -euo pipefail
[[ $(uname -s) == Linux && -f flake.lock && -f packages/bterm/Cargo.toml ]]
lane=$(cd "$(dirname "$0")" && pwd)
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET XAUTHORITY DBUS_SESSION_BUS_ADDRESS
rev=$(python3 -c 'import json; n=json.load(open("flake.lock"))["nodes"]; print(n[n["root"]["inputs"]["nixpkgs"]]["locked"]["rev"])')
sha256sum flake.lock Cargo.lock "$lane"/product.{sh,py}
nix develop --no-write-lock-file --command bash -eu -o pipefail -c '
  cargo build --locked -p bmux_cli -p bterm -p bmux_pane_runtime_plugin --bins
  pkgs=github:NixOS/nixpkgs/$2
  mesa=$(nix build --no-link --print-out-paths "$pkgs#mesa.drivers")
  shopt -s nullglob; drivers=("$mesa"/share/vulkan/icd.d/lvp*.json)
  [[ ${#drivers[@]} -eq 1 ]] || { echo "no unique lavapipe ICD in $mesa" >&2; exit 1; }
  export VK_DRIVER_FILES=${drivers[0]}
  sha256sum "$CARGO_TARGET_DIR"/debug/{bmux,bterm} "$VK_DRIVER_FILES"
  nix shell "$pkgs#xorg-server" "$pkgs#xauth" "$pkgs#xdotool" "$pkgs#xclip" --command python3 "$1/product.py" "$CARGO_TARGET_DIR/debug"
' _ "$lane" "$rev"
