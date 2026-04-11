#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MATCHER="$ROOT_DIR/scripts/plugin-runtime-ci-matcher.sh"

if [[ ! -x "$MATCHER" ]]; then
	echo "matcher script is not executable: $MATCHER" >&2
	exit 2
fi

check_match() {
	local path="$1"
	if ! "$MATCHER" --match "$path"; then
		echo "expected match, got no match: $path" >&2
		exit 1
	fi
}

check_no_match() {
	local path="$1"
	if "$MATCHER" --match "$path"; then
		echo "expected no match, got match: $path" >&2
		exit 1
	fi
}

check_match "packages/plugin/src/loader.rs"
check_match "packages/plugin-sdk/src/process_runtime.rs"
check_match "plugins/plugin-cli-plugin/src/lib.rs"
check_match "packages/cli/src/runtime/plugin_runtime.rs"
check_match "scripts/perf-plugin-command-latency.sh"
check_match "scripts/perf-plugin-runtime-matrix.sh"
check_match ".github/workflows/ci.yml"

check_no_match "docs/README.md"
check_no_match "packages/server/src/lib.rs"
check_no_match "scripts/smoke-pty-runtime.sh"

echo "plugin/runtime matcher fixtures passed"
