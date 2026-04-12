#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_SCRIPT="$ROOT_DIR/scripts/perf-plugin-command-latency.sh"

BMUX_BIN="${BMUX_BIN:-}"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"
ITERATIONS="${ITERATIONS:-30}"
WARMUP="${WARMUP:-5}"
COLD_MODE="0"
ARTIFACT_DIR="${ARTIFACT_DIR:-}"
SCALE_PLUGIN_COUNT="${SCALE_PLUGIN_COUNT:-0}"
SCALE_PROFILE="${SCALE_PROFILE:-}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-runtime-matrix.sh [options]

Runs plugin usability/runtime latency checks across key command paths.

Options:
  --bmux-bin PATH     Use an explicit bmux executable
  --iterations N      Measured iterations per scenario (default: 30)
  --warmup N          Warmup iterations per scenario (default: 5)
  --cold              Run without warmup (sets warmup to 0)
  --artifact-dir DIR  Write per-scenario JSON artifact reports
  --scale-plugin-count N  Generate N synthetic plugin manifests for scale scenarios
  --scale-profile NAME    Synthetic profile: small, medium, or large
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
		--cold)
			COLD_MODE="1"
			shift
			;;
		--artifact-dir)
			ARTIFACT_DIR="$2"
			shift 2
			;;
		--scale-plugin-count)
			SCALE_PLUGIN_COUNT="$2"
			shift 2
			;;
		--scale-profile)
			SCALE_PROFILE="$2"
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
	local max_steady_p95_ms="$4"
	local max_steady_p99_ms="$5"
	shift 5
	local args=("$@")

	if [[ "$COLD_MODE" == "1" ]]; then
		max_p95_ms=$((max_p95_ms * 20))
		max_p99_ms=$((max_p99_ms * 20))
	fi

	local artifact_json=""
	if [[ -n "$ARTIFACT_DIR" ]]; then
		local slug
		slug="$(printf '%s' "$title" | tr '[:upper:]' '[:lower:]' | tr ' ' '-' | tr -cd 'a-z0-9-')"
		artifact_json="$ARTIFACT_DIR/${slug}.json"
	fi

	echo
	echo "=== ${title} ==="
	local cmd=(
		"$BASE_SCRIPT"
		--iterations "$ITERATIONS"
		--warmup "$WARMUP"
		--max-p95-ms "$max_p95_ms"
		--max-p99-ms "$max_p99_ms"
		--max-steady-p95-ms "$max_steady_p95_ms"
		--max-steady-p99-ms "$max_steady_p99_ms"
		--max-runtime-retries 0
		--max-runtime-respawns 0
		--max-runtime-timeouts 0
	)
	if [[ -n "$BMUX_BIN" ]]; then
		cmd+=(--bmux-bin "$BMUX_BIN")
	fi
	if [[ -n "$artifact_json" ]]; then
		cmd+=(--artifact-json "$artifact_json")
	fi
	if [[ "$SCALE_PLUGIN_COUNT" -gt 0 ]]; then
		cmd+=(--scale-plugin-count "$SCALE_PLUGIN_COUNT")
	fi
	if [[ -n "$SCALE_PROFILE" ]]; then
		cmd+=(--scale-profile "$SCALE_PROFILE")
	fi
	cmd+=(-- "${args[@]}")
	"${cmd[@]}"
}

run_case_allow_nonzero() {
	local title="$1"
	local max_p95_ms="$2"
	local max_p99_ms="$3"
	local max_steady_p95_ms="$4"
	local max_steady_p99_ms="$5"
	shift 5
	local args=("$@")

	if [[ "$COLD_MODE" == "1" ]]; then
		max_p95_ms=$((max_p95_ms * 20))
		max_p99_ms=$((max_p99_ms * 20))
	fi

	local artifact_json=""
	if [[ -n "$ARTIFACT_DIR" ]]; then
		local slug
		slug="$(printf '%s' "$title" | tr '[:upper:]' '[:lower:]' | tr ' ' '-' | tr -cd 'a-z0-9-')"
		artifact_json="$ARTIFACT_DIR/${slug}.json"
	fi

	echo
	echo "=== ${title} ==="
	local cmd=(
		"$BASE_SCRIPT"
		--allow-nonzero
		--iterations "$ITERATIONS"
		--warmup "$WARMUP"
		--max-p95-ms "$max_p95_ms"
		--max-p99-ms "$max_p99_ms"
		--max-steady-p95-ms "$max_steady_p95_ms"
		--max-steady-p99-ms "$max_steady_p99_ms"
		--max-runtime-retries 0
		--max-runtime-respawns 0
		--max-runtime-timeouts 0
	)
	if [[ -n "$BMUX_BIN" ]]; then
		cmd+=(--bmux-bin "$BMUX_BIN")
	fi
	if [[ -n "$artifact_json" ]]; then
		cmd+=(--artifact-json "$artifact_json")
	fi
	if [[ "$SCALE_PLUGIN_COUNT" -gt 0 ]]; then
		cmd+=(--scale-plugin-count "$SCALE_PLUGIN_COUNT")
	fi
	if [[ -n "$SCALE_PROFILE" ]]; then
		cmd+=(--scale-profile "$SCALE_PROFILE")
	fi
	cmd+=(-- "${args[@]}")
	"${cmd[@]}"
}

find_happy_plugin_run_args() {
	"$BMUX_PERF_TOOLS_BIN" discover-run-candidate --bmux-bin "$BMUX_BIN"
}

parse_args "$@"
require_number "$ITERATIONS" "--iterations"
require_number "$WARMUP" "--warmup"
require_number "$SCALE_PLUGIN_COUNT" "--scale-plugin-count"

if [[ -n "$SCALE_PROFILE" ]]; then
	case "$SCALE_PROFILE" in
	small | medium | large) ;;
	*)
		echo "--scale-profile must be one of: small, medium, large" >&2
		exit 2
		;;
	esac
fi

if [[ "$SCALE_PLUGIN_COUNT" -eq 0 && -n "$SCALE_PROFILE" ]]; then
	case "$SCALE_PROFILE" in
	small) SCALE_PLUGIN_COUNT=40 ;;
	medium) SCALE_PLUGIN_COUNT=120 ;;
	large) SCALE_PLUGIN_COUNT=300 ;;
	esac
fi

if [[ "$COLD_MODE" == "1" ]]; then
	WARMUP=0
fi

if [[ ! -x "$BASE_SCRIPT" ]]; then
	echo "missing executable script: $BASE_SCRIPT" >&2
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

if [[ -z "$BMUX_BIN" ]]; then
	cargo build -q -p bmux_cli
	BMUX_BIN="$ROOT_DIR/target/debug/bmux"
fi

if [[ -n "$ARTIFACT_DIR" ]]; then
	mkdir -p "$ARTIFACT_DIR"
fi

if [[ ! -x "$BMUX_BIN" ]]; then
	echo "bmux binary not executable: $BMUX_BIN" >&2
	exit 2
fi

if [[ "$SCALE_PLUGIN_COUNT" -gt 0 ]]; then
	echo "scale fixture enabled: count=$SCALE_PLUGIN_COUNT profile=${SCALE_PROFILE:-medium}"
fi

run_case "plugin list json" 250 350 250 350 plugin list --json
run_case "plugin doctor json" 350 500 350 500 plugin doctor --json
run_case "plugin rebuild list json" 550 750 550 750 plugin rebuild --list --json
run_case_allow_nonzero "plugin run missing plugin" 350 550 350 550 plugin run missing.plugin-id no-op

if [[ "$SCALE_PLUGIN_COUNT" -gt 0 ]]; then
	run_case "plugin doctor json scale" 900 1300 850 1200 plugin doctor --json
	run_case "plugin rebuild list json scale" 1200 1700 1100 1600 plugin rebuild --list --json
fi

if happy_args=$(find_happy_plugin_run_args); then
	mapfile -t parts <<<"$happy_args"
	run_case "plugin run discovered command" 450 650 450 650 plugin run "${parts[0]}" "${parts[1]}"
else
	echo
	echo "=== plugin run discovered command ==="
	echo "Skipping: no successful plugin run candidate discovered in this environment"
fi

echo
echo "plugin runtime matrix perf checks passed"
