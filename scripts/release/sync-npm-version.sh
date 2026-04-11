#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
	echo "Usage: $0 <version>"
	exit 1
fi

VERSION="$1"

MAIN_PACKAGES=(
	"npm/bmux/package.json"
	"npm/cli/package.json"
)

PLATFORM_PACKAGES=(
	"npm/darwin-arm64/package.json"
	"npm/darwin-x64/package.json"
	"npm/linux-arm64/package.json"
	"npm/linux-x64/package.json"
	"npm/linux-x64-musl/package.json"
	"npm/win32-x64/package.json"
)

for pkg in "${PLATFORM_PACKAGES[@]}"; do
	jq --arg v "$VERSION" '.version = $v' "$pkg" >"$pkg.tmp"
	mv "$pkg.tmp" "$pkg"
done

for pkg in "${MAIN_PACKAGES[@]}"; do
	jq --arg v "$VERSION" '
    .version = $v
    | .optionalDependencies |= with_entries(.value = $v)
  ' "$pkg" >"$pkg.tmp"
	mv "$pkg.tmp" "$pkg"
done
