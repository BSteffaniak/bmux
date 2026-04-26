#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ITERATIONS="${ITERATIONS:-12}"
WARMUP="${WARMUP:-2}"
SWITCHES="${SWITCHES:-6}"
OUT_DIR="${OUT_DIR:-}"
SERVICE_TIMING=0
STORAGE_TIMING=0
IPC_TIMING=0

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-attach-scenario-matrix.sh [options]

Runs a closed-loop attach scenario matrix and writes one artifact per scenario.
Scenarios cover next-window, prev-window, goto-window, and new-window across
small and larger tab counts.

Options:
  --iterations N       Measured iterations per scenario (default: 12)
  --warmup N           Warmup iterations per scenario (default: 2)
  --switches N         Actions per playbook run (default: 6)
  --out-dir PATH       Artifact directory (default: temp dir)
  --service-timing     Include generic InvokeService timing
  --storage-timing     Include storage/volatile timing
  --ipc-timing         Include IPC timing
  -h, --help           Show this help message
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
	--switches)
		SWITCHES="$2"
		shift 2
		;;
	--out-dir)
		OUT_DIR="$2"
		shift 2
		;;
	--service-timing)
		SERVICE_TIMING=1
		shift
		;;
	--storage-timing)
		STORAGE_TIMING=1
		shift
		;;
	--ipc-timing)
		IPC_TIMING=1
		shift
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
require_number "$SWITCHES" "--switches"

if [[ -z "$OUT_DIR" ]]; then
	OUT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bmux-attach-matrix.XXXXXX")"
fi
mkdir -p "$OUT_DIR"

common=(
	--iterations "$ITERATIONS"
	--warmup "$WARMUP"
	--switches "$SWITCHES"
)
if [[ "$SERVICE_TIMING" -eq 1 ]]; then
	common+=(--service-timing)
fi
if [[ "$STORAGE_TIMING" -eq 1 ]]; then
	common+=(--storage-timing)
fi
if [[ "$IPC_TIMING" -eq 1 ]]; then
	common+=(--ipc-timing)
fi

run_case() {
	local scenario="$1"
	local windows="$2"
	local artifact="$OUT_DIR/${scenario}-${windows}.json"
	echo "scenario=${scenario} windows=${windows} artifact=${artifact}"
	MAX_ATTACH_COMMAND_P99_MS=50 MAX_RETARGET_P99_MS=50 \
		"$ROOT_DIR/scripts/perf-attach-tab-switch.sh" \
		"${common[@]}" \
		--scenario "$scenario" \
		--windows "$windows" \
		--artifact-json "$artifact"
}

run_case next-window 2
run_case next-window 4
run_case next-window 20
run_case prev-window 4
run_case goto-window 4
run_case goto-window 20
run_case new-window 2

echo "matrix_artifacts=$OUT_DIR"
