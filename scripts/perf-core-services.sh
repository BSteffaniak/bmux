#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ITERATIONS="${ITERATIONS:-1000}"
WARMUP="${WARMUP:-100}"
ARTIFACT_JSON="${ARTIFACT_JSON:-}"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"
MAX_P99_US="${MAX_P99_US:-}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-core-services.sh [options]

Measures host core service substrate costs without the attach UI:
durable storage cold/cached get, durable set, volatile get/set/clear, and
direct static service dispatch.

Options:
  --iterations N          Measured iterations (default: 1000)
  --warmup N              Warmup iterations (default: 100)
  --artifact-json PATH    Write machine-readable JSON artifact
  --max-p99-us N          Fail if any scenario p99 exceeds N us
  -h, --help              Show this help message
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
	--artifact-json)
		ARTIFACT_JSON="$2"
		shift 2
		;;
	--max-p99-us)
		MAX_P99_US="$2"
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
if [[ -n "$MAX_P99_US" ]]; then
	require_number "$MAX_P99_US" "--max-p99-us"
fi

cd "$ROOT_DIR"

if [[ -z "$BMUX_PERF_TOOLS_BIN" ]]; then
	cargo build -q -p bmux_perf_tools
	BMUX_PERF_TOOLS_BIN="$ROOT_DIR/target/debug/bmux-perf-tools"
fi
if [[ ! -x "$BMUX_PERF_TOOLS_BIN" ]]; then
	echo "bmux perf tools binary not executable: $BMUX_PERF_TOOLS_BIN" >&2
	exit 2
fi

if [[ -z "$ARTIFACT_JSON" ]]; then
	ARTIFACT_JSON="$(mktemp "${TMPDIR:-/tmp}/bmux-core-services.XXXXXX.json")"
fi

cmd=(
	"$BMUX_PERF_TOOLS_BIN" sample-core-services
	--iterations "$ITERATIONS"
	--warmup "$WARMUP"
	--out-json "$ARTIFACT_JSON"
)
if [[ -n "$MAX_P99_US" ]]; then
	cmd+=(--max-p99-us "$MAX_P99_US")
fi

"${cmd[@]}"
echo "artifact_json=$ARTIFACT_JSON"
