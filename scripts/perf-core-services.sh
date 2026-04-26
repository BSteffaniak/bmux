#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ITERATIONS="${ITERATIONS:-1000}"
WARMUP="${WARMUP:-100}"
ARTIFACT_JSON="${ARTIFACT_JSON:-}"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"
MAX_P99_US="${MAX_P99_US:-}"
CONFIG_PATH="${CONFIG_PATH:-$ROOT_DIR/perf/core-services.toml}"
PHASE_REPORT_DIR="${PHASE_REPORT_DIR:-}"
PROFILE="${PROFILE:-normal}"
MAX_P99_SET=0

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
  --config PATH           Phase validation config (default: perf/core-services.toml)
  --phase-report-dir PATH Write per-report phase artifacts to PATH
  --profile NAME          normal|diagnostic|ci|stress (default: normal)
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
		MAX_P99_SET=1
		shift 2
		;;
	--config)
		CONFIG_PATH="$2"
		shift 2
		;;
	--phase-report-dir)
		PHASE_REPORT_DIR="$2"
		shift 2
		;;
	--profile)
		PROFILE="$2"
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

case "$PROFILE" in
normal | ci)
	;;
diagnostic | stress)
	if [[ "$MAX_P99_SET" -eq 0 ]]; then
		MAX_P99_US=1000000000
	fi
	;;
*)
	echo "unknown --profile: $PROFILE" >&2
	usage
	exit 2
	;;
esac

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
if [[ -z "$PHASE_REPORT_DIR" ]]; then
	PHASE_REPORT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bmux-core-services-phases.XXXXXX")"
fi

cmd=(
	"$BMUX_PERF_TOOLS_BIN" sample-core-services
	--iterations "$ITERATIONS"
	--warmup "$WARMUP"
	--out-json "$ARTIFACT_JSON"
)

"${cmd[@]}"

validate_cmd=(
	"$BMUX_PERF_TOOLS_BIN" validate-phase-config
	--input "$ARTIFACT_JSON"
	--config "$CONFIG_PATH"
	--output-dir "$PHASE_REPORT_DIR"
)
if [[ -n "$MAX_P99_US" ]]; then
	MAX_P99_MS="$((MAX_P99_US / 1000)).$(printf '%03d' "$((MAX_P99_US % 1000))")"
	validate_cmd+=(--limit "core_service=$MAX_P99_MS")
fi
"${validate_cmd[@]}"
echo "artifact_json=$ARTIFACT_JSON"
echo "phase_report_dir=$PHASE_REPORT_DIR"
