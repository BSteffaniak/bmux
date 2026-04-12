#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"
BASELINE_DIR=""
CANDIDATE_DIR=""
WARN_REGRESSION_MS="10"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-artifact-compare.sh [options]

Compares perf artifact JSON reports against baseline JSON reports.
Missing baselines are skipped with a note.

Options:
  --baseline-dir DIR        Baseline JSON directory
  --candidate-dir DIR       Candidate JSON directory
  --warn-regression-ms N    Mark WARN when metric regresses by more than N ms (default: 10)
  --bmux-perf-tools-bin PATH  Explicit bmux-perf-tools binary
  -h, --help                Show this help message
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
		--baseline-dir)
			BASELINE_DIR="$2"
			shift 2
			;;
		--candidate-dir)
			CANDIDATE_DIR="$2"
			shift 2
			;;
		--warn-regression-ms)
			WARN_REGRESSION_MS="$2"
			shift 2
			;;
		--bmux-perf-tools-bin)
			BMUX_PERF_TOOLS_BIN="$2"
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

parse_args "$@"
require_number "$WARN_REGRESSION_MS" "--warn-regression-ms"

if [[ -z "$BASELINE_DIR" || -z "$CANDIDATE_DIR" ]]; then
	echo "--baseline-dir and --candidate-dir are required" >&2
	usage
	exit 2
fi

if [[ ! -d "$CANDIDATE_DIR" ]]; then
	echo "candidate directory does not exist: $CANDIDATE_DIR" >&2
	exit 2
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

echo "perf artifact compare: baseline=$BASELINE_DIR candidate=$CANDIDATE_DIR"

shopt -s nullglob
for candidate in "$CANDIDATE_DIR"/*.json; do
	name="$(basename "$candidate")"
	baseline="$BASELINE_DIR/$name"
	echo
	echo "=== compare $name ==="
	if [[ ! -f "$baseline" ]]; then
		echo "skip: no baseline file at $baseline"
		continue
	fi
	"$BMUX_PERF_TOOLS_BIN" compare-report \
		--baseline "$baseline" \
		--candidate "$candidate" \
		--warn-regression-ms "$WARN_REGRESSION_MS"
done

echo
echo "perf artifact compare complete"
