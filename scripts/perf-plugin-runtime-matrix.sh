#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_SCRIPT="$ROOT_DIR/scripts/perf-plugin-command-latency.sh"

BMUX_BIN="${BMUX_BIN:-}"
ITERATIONS="${ITERATIONS:-30}"
WARMUP="${WARMUP:-5}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-runtime-matrix.sh [options]

Runs plugin usability/runtime latency checks across key command paths.

Options:
  --bmux-bin PATH     Use an explicit bmux executable
  --iterations N      Measured iterations per scenario (default: 30)
  --warmup N          Warmup iterations per scenario (default: 5)
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

parse_args() {
	while (($# > 0)); do
		case "$1" in
		--bmux-bin)
			BMUX_BIN="$2"
			shift 2
			;;
		--iterations)
			ITERATIONS="$2"
			shift 2
			;;
		--warmup)
			WARMUP="$2"
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
}

run_case() {
	local title="$1"
	local max_p95_ms="$2"
	local max_p99_ms="$3"
	shift 3
	local args=("$@")

	echo
	echo "=== ${title} ==="
	local cmd=(
		"$BASE_SCRIPT"
		--iterations "$ITERATIONS"
		--warmup "$WARMUP"
		--max-p95-ms "$max_p95_ms"
		--max-p99-ms "$max_p99_ms"
	)
	if [[ -n "$BMUX_BIN" ]]; then
		cmd+=(--bmux-bin "$BMUX_BIN")
	fi
	cmd+=(-- "${args[@]}")
	"${cmd[@]}"
}

parse_args "$@"
require_number "$ITERATIONS" "--iterations"
require_number "$WARMUP" "--warmup"

if [[ ! -x "$BASE_SCRIPT" ]]; then
	echo "missing executable script: $BASE_SCRIPT" >&2
	exit 2
fi

run_case "plugin list json" 250 350 plugin list --json
run_case "plugin doctor json" 350 500 plugin doctor --json
run_case "plugin rebuild list json" 550 750 plugin rebuild --list --json

echo
echo "plugin runtime matrix perf checks passed"
