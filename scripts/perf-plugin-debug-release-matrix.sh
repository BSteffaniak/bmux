#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ITERATIONS="${ITERATIONS:-20}"
WARMUP="${WARMUP:-5}"
ARTIFACT_DIR="${ARTIFACT_DIR:-}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-debug-release-matrix.sh [options]

Runs the plugin runtime latency matrix against debug and release bmux binaries.

Options:
  --iterations N      Measured iterations per scenario (default: 20)
  --warmup N          Warmup iterations per scenario (default: 5)
  --artifact-dir DIR  Write artifacts under DIR/{debug,release}
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
	--artifact-dir)
		ARTIFACT_DIR="$2"
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

cd "$ROOT_DIR"

cargo build -q -p bmux_cli
cargo build -q -p bmux_cli --release

run_matrix() {
	local profile="$1"
	local bin="$2"
	local args=(
		"$ROOT_DIR/scripts/perf-plugin-runtime-matrix.sh"
		--bmux-bin "$bin"
		--iterations "$ITERATIONS"
		--warmup "$WARMUP"
	)
	if [[ -n "$ARTIFACT_DIR" ]]; then
		args+=(--artifact-dir "$ARTIFACT_DIR/$profile")
	fi

	echo
	echo "=== ${profile} plugin runtime matrix ==="
	"${args[@]}"
}

run_matrix debug "$ROOT_DIR/target/debug/bmux"
run_matrix release "$ROOT_DIR/target/release/bmux"

echo
