#!/usr/bin/env bash
# Run inside the pinned Nix shell after the coordinator's serialized release build.
set -euo pipefail
if [[ $# != 2 ]]; then echo 'usage: bash package-rootfs.sh /absolute/release/bmux-live-relay output-rootfs' >&2; exit 2; fi
binary=$(realpath "$1")
output=$2
[[ -x "$binary" ]] || { echo 'release executable missing' >&2; exit 1; }
[[ ! -e "$output" ]] || { echo 'output must not already exist' >&2; exit 1; }
# ldd is used only on our explicitly owned build artifact, never external input.
dependencies=$(ldd "$binary")
[[ "$dependencies" == *'/nix/store/'* ]] || { echo 'expected a dynamically linked Nix build' >&2; exit 1; }
if grep -q 'not found' <<< "$dependencies"; then echo 'unresolved ELF dependency' >&2; exit 1; fi
mkdir -p "$output/bin" "$output/nix/store" "$output/data" "$output/tmp"
cp "$binary" "$output/bin/bmux-live-relay"
mapfile -t roots < <(grep -o '/nix/store/[^ /]*' <<< "$dependencies" | sort -u)
[[ ${#roots[@]} -gt 0 ]] || { echo 'runtime closure unavailable' >&2; exit 1; }
mapfile -t closure < <(nix-store --query --requisites "${roots[@]}" | sort -u)
[[ ${#closure[@]} -gt 0 ]] || { echo 'empty Nix closure' >&2; exit 1; }
for item in "${closure[@]}"; do cp -a "$item" "$output/nix/store/"; done
chmod 1777 "$output/tmp"
chmod 755 "$output/data"
sha256sum "$output/bin/bmux-live-relay"
printf 'Packaged %s store paths; build with docker build -f packages/live-relay/Dockerfile .\n' "${#closure[@]}"
