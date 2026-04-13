#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNS="3"
ITERATIONS="8"
WARMUP="2"
OUT_DIR="${OUT_DIR:-/tmp/bmux-plugin-perf-variance}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-variance.sh [options]

Runs repeated runtime-matrix samples and compares all runs against perf baselines
to classify likely noise vs likely regression.

Options:
  --runs N         Number of repeated runs (default: 3)
  --iterations N   Iterations per run (default: 8)
  --warmup N       Warmup samples per run (default: 2)
  --out-dir DIR    Output directory for repeated artifacts
  -h, --help       Show this help message
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
		--runs)
			RUNS="$2"
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
		--out-dir)
			OUT_DIR="$2"
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
require_number "$RUNS" "--runs"
require_number "$ITERATIONS" "--iterations"
require_number "$WARMUP" "--warmup"

if [[ "$RUNS" -lt 2 ]]; then
	echo "--runs must be at least 2 for variance analysis" >&2
	exit 2
fi

cd "$ROOT_DIR"
mkdir -p "$OUT_DIR"

echo "plugin perf variance: runs=$RUNS iterations=$ITERATIONS warmup=$WARMUP out=$OUT_DIR"

for run in $(seq 1 "$RUNS"); do
	run_dir="$OUT_DIR/run-$run"
	mkdir -p "$run_dir"
	echo
	echo "=== runtime matrix run $run/$RUNS ==="
	./scripts/perf-plugin-runtime-matrix.sh \
		--iterations "$ITERATIONS" \
		--warmup "$WARMUP" \
		--artifact-dir "$run_dir"
done

extra_args=()
for run in $(seq 2 "$RUNS"); do
	extra_args+=(--extra-candidate-dir "$OUT_DIR/run-$run")
done

echo
echo "=== aggregate compare vs baselines ==="
./scripts/perf-plugin-artifact-compare.sh \
	--candidate-dir "$OUT_DIR/run-1" \
	--baseline-dir "docs/perf-baselines/runtime-matrix" \
	--warn-regression-ms 20 \
	"${extra_args[@]}"

echo
echo "plugin perf variance complete"
