#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ITERATIONS="${ITERATIONS:-60}"
WARMUP="${WARMUP:-10}"
MAX_P99_MS="${MAX_P99_MS:-}"
MAX_P95_MS="${MAX_P95_MS:-}"
MAX_AVG_MS="${MAX_AVG_MS:-}"
MAX_STEADY_P99_MS="${MAX_STEADY_P99_MS:-}"
MAX_STEADY_P95_MS="${MAX_STEADY_P95_MS:-}"
MAX_STEADY_AVG_MS="${MAX_STEADY_AVG_MS:-}"
MAX_RUNTIME_RETRIES="${MAX_RUNTIME_RETRIES:-}"
MAX_RUNTIME_RESPAWNS="${MAX_RUNTIME_RESPAWNS:-}"
MAX_RUNTIME_TIMEOUTS="${MAX_RUNTIME_TIMEOUTS:-}"
ALLOW_NONZERO="0"

BMUX_BIN="${BMUX_BIN:-}"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"
ARTIFACT_JSON="${ARTIFACT_JSON:-}"
SCALE_PLUGIN_COUNT="${SCALE_PLUGIN_COUNT:-0}"
SCALE_PROFILE="${SCALE_PROFILE:-}"
TARGET_ARGS=(plugin list --json)

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-command-latency.sh [options] [-- <bmux args...>]

Measures end-to-end command latency (process startup + command execution) and
prints p50/p95/p99 in milliseconds.

Options:
  --iterations N      Measured iterations (default: 60)
  --warmup N          Warmup iterations (default: 10)
  --bmux-bin PATH     Use an explicit bmux executable path
  --max-p99-ms N      Fail if measured p99 is above N milliseconds
  --max-p95-ms N      Fail if measured p95 is above N milliseconds
  --max-avg-ms N      Fail if measured average is above N milliseconds
  --max-steady-p99-ms N  Fail if steady-state p99 is above N milliseconds
  --max-steady-p95-ms N  Fail if steady-state p95 is above N milliseconds
  --max-steady-avg-ms N  Fail if steady-state average is above N milliseconds
  --max-runtime-retries N   Fail if runtime retry warnings exceed N
  --max-runtime-respawns N  Fail if runtime respawn warnings exceed N
  --max-runtime-timeouts N  Fail if runtime timeout warnings exceed N
  --artifact-json PATH      Write machine-readable JSON artifact report
  --scale-plugin-count N    Generate N synthetic plugin manifests for scale scenarios
  --scale-profile NAME      Synthetic profile: small, medium, or large
  --allow-nonzero     Allow non-zero command exit status during sampling
  -h, --help          Show this help message

Examples:
  ./scripts/perf-plugin-command-latency.sh
  ./scripts/perf-plugin-command-latency.sh --iterations 100 --max-p99-ms 40
  ./scripts/perf-plugin-command-latency.sh -- --logs path
USAGE
}

parse_args() {
	local positional_mode=0
	while (($# > 0)); do
		if [[ "$positional_mode" == "1" ]]; then
			TARGET_ARGS+=("$1")
			shift
			continue
		fi

		case "$1" in
		--iterations)
			ITERATIONS="$2"
			shift 2
			;;
		--warmup)
			WARMUP="$2"
			shift 2
			;;
		--max-p99-ms)
			MAX_P99_MS="$2"
			shift 2
			;;
		--bmux-bin)
			BMUX_BIN="$2"
			shift 2
			;;
		--max-p95-ms)
			MAX_P95_MS="$2"
			shift 2
			;;
		--max-avg-ms)
			MAX_AVG_MS="$2"
			shift 2
			;;
		--max-steady-p99-ms)
			MAX_STEADY_P99_MS="$2"
			shift 2
			;;
		--max-steady-p95-ms)
			MAX_STEADY_P95_MS="$2"
			shift 2
			;;
		--max-steady-avg-ms)
			MAX_STEADY_AVG_MS="$2"
			shift 2
			;;
		--allow-nonzero)
			ALLOW_NONZERO="1"
			shift
			;;
		--max-runtime-retries)
			MAX_RUNTIME_RETRIES="$2"
			shift 2
			;;
		--max-runtime-respawns)
			MAX_RUNTIME_RESPAWNS="$2"
			shift 2
			;;
		--max-runtime-timeouts)
			MAX_RUNTIME_TIMEOUTS="$2"
			shift 2
			;;
		--artifact-json)
			ARTIFACT_JSON="$2"
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
		--)
			positional_mode=1
			TARGET_ARGS=()
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
}

require_number() {
	local value="$1"
	local name="$2"
	if ! [[ "$value" =~ ^[0-9]+$ ]]; then
		echo "${name} must be a non-negative integer" >&2
		exit 2
	fi
}

parse_args "$@"

require_number "$ITERATIONS" "--iterations"
require_number "$WARMUP" "--warmup"

if [[ -n "$MAX_P99_MS" ]]; then
	require_number "$MAX_P99_MS" "--max-p99-ms"
fi
if [[ -n "$MAX_P95_MS" ]]; then
	require_number "$MAX_P95_MS" "--max-p95-ms"
fi
if [[ -n "$MAX_AVG_MS" ]]; then
	require_number "$MAX_AVG_MS" "--max-avg-ms"
fi
if [[ -n "$MAX_STEADY_P99_MS" ]]; then
	require_number "$MAX_STEADY_P99_MS" "--max-steady-p99-ms"
fi
if [[ -n "$MAX_STEADY_P95_MS" ]]; then
	require_number "$MAX_STEADY_P95_MS" "--max-steady-p95-ms"
fi
if [[ -n "$MAX_STEADY_AVG_MS" ]]; then
	require_number "$MAX_STEADY_AVG_MS" "--max-steady-avg-ms"
fi
if [[ -n "$MAX_RUNTIME_RETRIES" ]]; then
	require_number "$MAX_RUNTIME_RETRIES" "--max-runtime-retries"
fi
if [[ -n "$MAX_RUNTIME_RESPAWNS" ]]; then
	require_number "$MAX_RUNTIME_RESPAWNS" "--max-runtime-respawns"
fi
if [[ -n "$MAX_RUNTIME_TIMEOUTS" ]]; then
	require_number "$MAX_RUNTIME_TIMEOUTS" "--max-runtime-timeouts"
fi
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

if [[ "${#TARGET_ARGS[@]}" -eq 0 ]]; then
	echo "expected bmux command args after --" >&2
	usage
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

if [[ ! -x "$BMUX_BIN" ]]; then
	echo "bmux binary not executable: $BMUX_BIN" >&2
	exit 2
fi

SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/bmux-perf.XXXXXX")"
cleanup() {
	rm -rf "$SANDBOX"
}
trap cleanup EXIT

export XDG_CONFIG_HOME="$SANDBOX/config"
export XDG_DATA_HOME="$SANDBOX/data"
export XDG_RUNTIME_DIR="$SANDBOX/runtime"
export BMUX_STATE_DIR="$SANDBOX/state"
export BMUX_LOG_DIR="$SANDBOX/logs"
export TMPDIR="$SANDBOX/tmp"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR" "$BMUX_STATE_DIR" "$BMUX_LOG_DIR" "$TMPDIR"

if [[ "$SCALE_PLUGIN_COUNT" -gt 0 ]]; then
	prepare_args=(
		"$BMUX_PERF_TOOLS_BIN"
		prepare-scale-fixture
		--config-dir "$XDG_CONFIG_HOME/bmux"
		--plugin-root "$XDG_DATA_HOME/perf-scale-plugins"
		--count "$SCALE_PLUGIN_COUNT"
	)
	if [[ -n "$SCALE_PROFILE" ]]; then
		prepare_args+=(--profile "$SCALE_PROFILE")
	fi
	"${prepare_args[@]}"
fi

echo "benchmarking: bmux ${TARGET_ARGS[*]}"
echo "iterations=${ITERATIONS} warmup=${WARMUP}"

for ((i = 0; i < WARMUP; i += 1)); do
	if [[ "$ALLOW_NONZERO" == "1" ]]; then
		"$BMUX_BIN" "${TARGET_ARGS[@]}" >/dev/null 2>&1 || true
	else
		"$BMUX_BIN" "${TARGET_ARGS[@]}" >/dev/null 2>&1
	fi
done

SAMPLE_JSON_FILE="$SANDBOX/sample.json"

"$BMUX_PERF_TOOLS_BIN" sample \
	--iterations "$ITERATIONS" \
	--allow-nonzero "$ALLOW_NONZERO" \
	--out-json "$SAMPLE_JSON_FILE" \
	-- "$BMUX_BIN" "${TARGET_ARGS[@]}"

latency_cmd=(
	"$BMUX_PERF_TOOLS_BIN"
	report-latency
	--input "$SAMPLE_JSON_FILE"
)
if [[ -n "$MAX_P99_MS" ]]; then
	latency_cmd+=(--max-p99-ms "$MAX_P99_MS")
fi
if [[ -n "$MAX_P95_MS" ]]; then
	latency_cmd+=(--max-p95-ms "$MAX_P95_MS")
fi
if [[ -n "$MAX_AVG_MS" ]]; then
	latency_cmd+=(--max-avg-ms "$MAX_AVG_MS")
fi
if [[ -n "$MAX_STEADY_P99_MS" ]]; then
	latency_cmd+=(--max-steady-p99-ms "$MAX_STEADY_P99_MS")
fi
if [[ -n "$MAX_STEADY_P95_MS" ]]; then
	latency_cmd+=(--max-steady-p95-ms "$MAX_STEADY_P95_MS")
fi
if [[ -n "$MAX_STEADY_AVG_MS" ]]; then
	latency_cmd+=(--max-steady-avg-ms "$MAX_STEADY_AVG_MS")
fi
"${latency_cmd[@]}"

fault_cmd=(
	"$BMUX_PERF_TOOLS_BIN"
	report-faults
	--input "$SAMPLE_JSON_FILE"
)
if [[ -n "$MAX_RUNTIME_RETRIES" ]]; then
	fault_cmd+=(--max-runtime-retries "$MAX_RUNTIME_RETRIES")
fi
if [[ -n "$MAX_RUNTIME_RESPAWNS" ]]; then
	fault_cmd+=(--max-runtime-respawns "$MAX_RUNTIME_RESPAWNS")
fi
if [[ -n "$MAX_RUNTIME_TIMEOUTS" ]]; then
	fault_cmd+=(--max-runtime-timeouts "$MAX_RUNTIME_TIMEOUTS")
fi
"${fault_cmd[@]}"

if [[ -n "$ARTIFACT_JSON" ]]; then
	artifact_cmd=(
		"$BMUX_PERF_TOOLS_BIN"
		report-json
		--input "$SAMPLE_JSON_FILE"
		--output "$ARTIFACT_JSON"
	)
	if [[ -n "$MAX_P99_MS" ]]; then
		artifact_cmd+=(--max-p99-ms "$MAX_P99_MS")
	fi
	if [[ -n "$MAX_P95_MS" ]]; then
		artifact_cmd+=(--max-p95-ms "$MAX_P95_MS")
	fi
	if [[ -n "$MAX_AVG_MS" ]]; then
		artifact_cmd+=(--max-avg-ms "$MAX_AVG_MS")
	fi
	if [[ -n "$MAX_STEADY_P99_MS" ]]; then
		artifact_cmd+=(--max-steady-p99-ms "$MAX_STEADY_P99_MS")
	fi
	if [[ -n "$MAX_STEADY_P95_MS" ]]; then
		artifact_cmd+=(--max-steady-p95-ms "$MAX_STEADY_P95_MS")
	fi
	if [[ -n "$MAX_STEADY_AVG_MS" ]]; then
		artifact_cmd+=(--max-steady-avg-ms "$MAX_STEADY_AVG_MS")
	fi
	if [[ -n "$MAX_RUNTIME_RETRIES" ]]; then
		artifact_cmd+=(--max-runtime-retries "$MAX_RUNTIME_RETRIES")
	fi
	if [[ -n "$MAX_RUNTIME_RESPAWNS" ]]; then
		artifact_cmd+=(--max-runtime-respawns "$MAX_RUNTIME_RESPAWNS")
	fi
	if [[ -n "$MAX_RUNTIME_TIMEOUTS" ]]; then
		artifact_cmd+=(--max-runtime-timeouts "$MAX_RUNTIME_TIMEOUTS")
	fi
	"${artifact_cmd[@]}"
fi

echo "perf check passed"
