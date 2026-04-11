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
DIST_DIR="$APT_ROOT/dists/$CHANNEL/main/binary-amd64"

mkdir -p "$POOL_DIR" "$DIST_DIR"

find "$ROOT/$CHANNEL/artifacts" -type f -name "*.deb" -exec cp -f {} "$POOL_DIR" \;

pushd "$APT_ROOT" >/dev/null
dpkg-scanpackages --multiversion pool >"$DIST_DIR/Packages"
gzip -9 -f -k "$DIST_DIR/Packages"

cat >"$APT_ROOT/dists/$CHANNEL/Release" <<EOF
Origin: bmux
Label: bmux
Suite: $CHANNEL
Codename: $CHANNEL
Architectures: amd64
Components: main
Description: bmux $CHANNEL apt repository
EOF

apt-ftparchive release "$APT_ROOT/dists/$CHANNEL" >>"$APT_ROOT/dists/$CHANNEL/Release"

if [ -n "${GPG_KEY_ID:-}" ]; then
	gpg --batch --yes --default-key "$GPG_KEY_ID" --clearsign -o "$APT_ROOT/dists/$CHANNEL/InRelease" "$APT_ROOT/dists/$CHANNEL/Release"
	gpg --batch --yes --default-key "$GPG_KEY_ID" -abs -o "$APT_ROOT/dists/$CHANNEL/Release.gpg" "$APT_ROOT/dists/$CHANNEL/Release"
fi
popd >/dev/null
