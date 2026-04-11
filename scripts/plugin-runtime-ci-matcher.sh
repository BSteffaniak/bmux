#!/usr/bin/env bash
set -euo pipefail

PATTERN='^(packages/plugin/|packages/plugin-sdk/|plugins/|packages/cli/src/runtime/plugin[^/]*\.rs$|scripts/perf-plugin-command-latency\.sh$|scripts/perf-plugin-runtime-matrix\.sh$|scripts/plugin-runtime-ci-matcher\.sh$|scripts/test-plugin-runtime-ci-matcher\.sh$|\.github/workflows/ci\.yml$)'

usage() {
	cat <<'USAGE'
Usage:
  plugin-runtime-ci-matcher.sh --match <path>
  plugin-runtime-ci-matcher.sh --any
  plugin-runtime-ci-matcher.sh --pattern

Options:
  --match <path>  Exit 0 if the path matches the plugin/runtime CI pattern.
  --any           Read newline-delimited paths from stdin; exit 0 if any match.
  --pattern       Print the matcher regex.
USAGE
}

if (($# == 0)); then
	usage
	exit 2
fi

case "$1" in
--match)
	if (($# != 2)); then
		usage
		exit 2
	fi
	if [[ "$2" =~ $PATTERN ]]; then
		exit 0
	fi
	exit 1
	;;
--any)
	while IFS= read -r path; do
		if [[ "$path" =~ $PATTERN ]]; then
			exit 0
		fi
	done
	exit 1
	;;
--pattern)
	printf '%s\n' "$PATTERN"
	;;
-h | --help)
	usage
	;;
*)
	usage
	exit 2
	;;
esac
