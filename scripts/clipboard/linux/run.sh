#!/usr/bin/env bash
# Run on nixos-desktop from the root of a disposable source copy, not a shared
# checkout. Shipping extracts d18b827936d2 itself. Use a separate disposable
# snapshot for the candidate. Invoke this script by absolute path.
# Usage: bash /path/to/run.sh shipping|candidate > /path/to/evidence.log 2>&1
set -euo pipefail
[[ $(uname -s) == Linux && -f packages/clipboard/Cargo.toml ]]
lane=$(cd "$(dirname "$0")" && pwd)
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET XAUTHORITY SWAYSOCK DBUS_SESSION_BUS_ADDRESS
case ${1:-} in
  shipping|candidate) ;;
  *) echo 'expected shipping or candidate' >&2; exit 2;;
esac
# Extract shipping directly from Git: never benchmark a dirty checkout as the reference.
if [[ $1 == shipping ]]; then
  shipping=$(mktemp -d)
  git archive d18b827936d2 | tar -xf - -C "$shipping"
  cd "$shipping"
  echo "shipping source: $shipping @ d18b827936d2"
fi
example=packages/clipboard/examples/lane_probe.rs
mkdir -p "$(dirname "$example")"
(set -C; cat "$lane/lane_probe.rs" > "$example")
trap 'rm -f "$example"' EXIT
sha256sum Cargo.lock flake.lock packages/clipboard/Cargo.toml "$example" "$lane/interop.py" packages/clipboard/src/{lib.rs,native/unix.rs,native/unix/*.rs}
rev=$(python3 -c 'import json; n=json.load(open("flake.lock"))["nodes"]; print(n[n["root"]["inputs"]["nixpkgs"]]["locked"]["rev"])')
pkgs="github:NixOS/nixpkgs/$rev"
nix develop --no-write-lock-file --command bash -eu -o pipefail -c '
  lane=$1; pkgs=$2; shift 2
  rustc -Vv
  cargo build --locked -p bmux_clipboard --example lane_probe
  probe=${CARGO_TARGET_DIR:?}/debug/examples/lane_probe
  export BMUX_LANE_NIXPKGS="$pkgs"
  python=$(nix build --no-link --print-out-paths --impure --expr '\''let pkgs = (builtins.getFlake (builtins.getEnv "BMUX_LANE_NIXPKGS")).legacyPackages.${builtins.currentSystem}; in pkgs.python3.withPackages (p: [ p.pillow ])'\'')
  nix shell "$pkgs#xorg-server" "$pkgs#xauth" "$pkgs#xclip" "$pkgs#sway" "$pkgs#wl-clipboard" --command "$python/bin/python3" "$lane/interop.py" "$probe"
' _ "$lane" "$pkgs"
