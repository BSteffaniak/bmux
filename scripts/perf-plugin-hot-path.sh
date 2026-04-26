#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ITERATIONS="${ITERATIONS:-1000}"
WARMUP="${WARMUP:-100}"
MAX_P99_US="${MAX_P99_US:-5000}"
ARTIFACT_JSON="${ARTIFACT_JSON:-}"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-hot-path.sh [options]

Measures in-process static plugin service dispatch without fresh bmux process
startup, filesystem discovery, or local IPC. This is the hot-path baseline for
plugin runtime overhead.

Options:
  --iterations N      Measured iterations (default: 1000)
  --warmup N          Warmup iterations (default: 100)
  --max-p99-us N      Fail if p99 exceeds N microseconds (default: 5000)
  --artifact-json P   Write machine-readable JSON artifact
  -h, --help          Show this help message
USAGE
}

require_number() {
	local value="$1"
	local name="$2"
	if ! [[ "$value" =~ ^[0-9]+$ ]]; then
		echo "${name} must be a non-negative integer" >&2
		exit 2
	fi
}

while (($# > 0)); do
	case "$1" in
	--iterations)
		ITERATIONS="$2"
		shift 2
		;;
	--warmup)
		WARMUP="$2"
		shift 2
		;;
	--max-p99-us)
		MAX_P99_US="$2"
		shift 2
		;;
	--artifact-json)
		ARTIFACT_JSON="$2"
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		usage
		exit 2
		;;
	esac
done

require_number "$ITERATIONS" "--iterations"
require_number "$WARMUP" "--warmup"
require_number "$MAX_P99_US" "--max-p99-us"

cd "$ROOT_DIR"

if [[ -z "$BMUX_PERF_TOOLS_BIN" ]]; then
	cargo build -q -p bmux_perf_tools
	BMUX_PERF_TOOLS_BIN="$ROOT_DIR/target/debug/bmux-perf-tools"
fi

if [[ -z "$ARTIFACT_JSON" ]]; then
	ARTIFACT_JSON="$(mktemp "${TMPDIR:-/tmp}/bmux-plugin-hot-path.XXXXXX.json")"
fi

"$BMUX_PERF_TOOLS_BIN" sample-static-service \
	--iterations "$ITERATIONS" \
	--warmup "$WARMUP" \
	--max-p99-us "$MAX_P99_US" \
	--out-json "$ARTIFACT_JSON"

echo "artifact_json=$ARTIFACT_JSON"
echo "plugin hot-path perf check passed"
