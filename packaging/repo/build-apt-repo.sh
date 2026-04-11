#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 2 ]; then
	echo "Usage: $0 <channel> <repo-root>"
	exit 1
fi

CHANNEL="$1"
ROOT="$2"

APT_ROOT="$ROOT/$CHANNEL/apt"
POOL_DIR="$APT_ROOT/pool/main/b/bmux"
DIST_BASE_DIR="$APT_ROOT/dists/$CHANNEL/main"

mkdir -p "$POOL_DIR" "$DIST_BASE_DIR/binary-amd64" "$DIST_BASE_DIR/binary-arm64"

find "$ROOT/$CHANNEL/artifacts" -type f -name "*.deb" -exec cp -f {} "$POOL_DIR" \;

pushd "$APT_ROOT" >/dev/null

rm -f "$DIST_BASE_DIR/binary-amd64/Packages" "$DIST_BASE_DIR/binary-arm64/Packages"

: >"$DIST_BASE_DIR/binary-amd64/Packages"
: >"$DIST_BASE_DIR/binary-arm64/Packages"
dpkg-scanpackages --arch amd64 --multiversion "$POOL_DIR" /dev/null >"$DIST_BASE_DIR/binary-amd64/Packages" || true
dpkg-scanpackages --arch arm64 --multiversion "$POOL_DIR" /dev/null >"$DIST_BASE_DIR/binary-arm64/Packages" || true

gzip -9 -f -k "$DIST_BASE_DIR/binary-amd64/Packages"
gzip -9 -f -k "$DIST_BASE_DIR/binary-arm64/Packages"

cat >"$APT_ROOT/dists/$CHANNEL/Release" <<EOF
Origin: bmux
Label: bmux
Suite: $CHANNEL
Codename: $CHANNEL
Architectures: amd64 arm64
Components: main
Description: bmux $CHANNEL apt repository
EOF

apt-ftparchive release "$APT_ROOT/dists/$CHANNEL" >>"$APT_ROOT/dists/$CHANNEL/Release"

if [ -n "${GPG_KEY_ID:-}" ]; then
	if [ -n "${GPG_PASSPHRASE:-}" ]; then
		GPG_ARGS=(--pinentry-mode loopback --passphrase "$GPG_PASSPHRASE")
	else
		GPG_ARGS=()
	fi
	gpg --batch --yes --default-key "$GPG_KEY_ID" "${GPG_ARGS[@]}" --clearsign -o "$APT_ROOT/dists/$CHANNEL/InRelease" "$APT_ROOT/dists/$CHANNEL/Release"
	gpg --batch --yes --default-key "$GPG_KEY_ID" "${GPG_ARGS[@]}" -abs -o "$APT_ROOT/dists/$CHANNEL/Release.gpg" "$APT_ROOT/dists/$CHANNEL/Release"
fi
popd >/dev/null
