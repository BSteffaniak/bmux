#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 2 ]; then
	echo "Usage: $0 <channel> <repo-root>"
	exit 1
fi

CHANNEL="$1"
ROOT="$2"

RPM_ROOT="$ROOT/$CHANNEL/rpm"
mkdir -p "$RPM_ROOT"

find "$ROOT/$CHANNEL/artifacts" -type f -name "*.rpm" -exec cp -f {} "$RPM_ROOT" \;

createrepo_c --update "$RPM_ROOT"

if [ -n "${GPG_KEY_ID:-}" ]; then
	if [ -n "${GPG_PASSPHRASE:-}" ]; then
		GPG_ARGS=(--pinentry-mode loopback --passphrase "$GPG_PASSPHRASE")
	else
		GPG_ARGS=()
	fi
	gpg --batch --yes --default-key "$GPG_KEY_ID" "${GPG_ARGS[@]}" --detach-sign --armor -o "$RPM_ROOT/repodata/repomd.xml.asc" "$RPM_ROOT/repodata/repomd.xml"
fi
