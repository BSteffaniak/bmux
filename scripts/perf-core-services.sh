#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-core-services.sh [options]

Compatibility wrapper for:
  bmux-perf-tools run-benchmark --manifest perf/core-services.toml

Common options:
  --iterations N
  --warmup N
  --artifact-json PATH
  --phase-report-dir PATH
  --profile normal|diagnostic|ci|stress
  --max-p99-us N
  -h, --help
USAGE
}

args=(--manifest "$ROOT_DIR/perf/core-services.toml")
while (($# > 0)); do
	case "$1" in
	--max-p99-us)
		value="$2"
		if ! [[ "$value" =~ ^[0-9]+$ ]]; then
			echo "--max-p99-us must be a non-negative integer" >&2
			exit 2
		fi
		args+=(--limit "core_service=$((value / 1000)).$(printf '%03d' "$((value % 1000))")")
		shift 2
		;;
	--config)
		args=(--manifest "$2")
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		args+=("$1")
		shift
		;;
	esac
done

cd "$ROOT_DIR"
if [[ -z "$BMUX_PERF_TOOLS_BIN" ]]; then
	cargo build -q -p bmux_perf_tools
	BMUX_PERF_TOOLS_BIN="$ROOT_DIR/target/debug/bmux-perf-tools"
fi

exec "$BMUX_PERF_TOOLS_BIN" run-benchmark "${args[@]}"
