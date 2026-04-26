#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ITERATIONS="${ITERATIONS:-30}"
WARMUP="${WARMUP:-5}"
WINDOWS="${WINDOWS:-4}"
SWITCHES="${SWITCHES:-4}"
MAX_P99_MS="${MAX_P99_MS:-}"
MAX_ATTACH_COMMAND_P99_MS="${MAX_ATTACH_COMMAND_P99_MS:-8}"
MAX_RETARGET_P99_MS="${MAX_RETARGET_P99_MS:-8}"
ARTIFACT_JSON="${ARTIFACT_JSON:-}"
BMUX_BIN="${BMUX_BIN:-}"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"
SERVICE_TIMING=0
IPC_TIMING=0

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-attach-tab-switch.sh [options]

Measures closed-loop attach tab switching through playbook `send-attach`, which
exercises keymap dispatch, plugin command execution, windows-plugin switching,
attach retargeting, viewport update, snapshot hydration, and status refresh.
After creating the requested windows, the playbook sends one unmeasured
prev-window action so the reported next-window samples represent steady-state
switching rather than the immediate post-create transition.

Options:
  --iterations N                  Measured iterations (default: 30)
  --warmup N                      Warmup iterations (default: 5)
  --windows N                     Number of windows/tabs in each playbook run (default: 4)
  --switches N                    Number of ctrl+s switches per playbook run (default: 4)
  --bmux-bin PATH                 Use an explicit bmux executable path
  --artifact-json PATH            Write machine-readable JSON artifact
  --service-timing                Include generic InvokeService client timing
  --ipc-timing                    Include generic IPC request timing
  --max-p99-ms N                  Fail if whole playbook p99 exceeds N ms
  --max-attach-command-p99-ms N   Fail if attach.plugin_command total p99 exceeds N ms (default: 8)
  --max-retarget-p99-ms N         Fail if attach.retarget_context total p99 exceeds N ms (default: 8)
  -h, --help                      Show this help message
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
	--windows)
		WINDOWS="$2"
		shift 2
		;;
	--switches)
		SWITCHES="$2"
		shift 2
		;;
	--bmux-bin)
		BMUX_BIN="$2"
		shift 2
		;;
	--artifact-json)
		ARTIFACT_JSON="$2"
		shift 2
		;;
	--service-timing)
		SERVICE_TIMING=1
		shift
		;;
	--ipc-timing)
		IPC_TIMING=1
		shift
		;;
	--max-p99-ms)
		MAX_P99_MS="$2"
		shift 2
		;;
	--max-attach-command-p99-ms)
		MAX_ATTACH_COMMAND_P99_MS="$2"
		shift 2
		;;
	--max-retarget-p99-ms)
		MAX_RETARGET_P99_MS="$2"
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
require_number "$WINDOWS" "--windows"
require_number "$SWITCHES" "--switches"
require_number "$MAX_ATTACH_COMMAND_P99_MS" "--max-attach-command-p99-ms"
require_number "$MAX_RETARGET_P99_MS" "--max-retarget-p99-ms"
if [[ -n "$MAX_P99_MS" ]]; then
	require_number "$MAX_P99_MS" "--max-p99-ms"
fi
if [[ "$WINDOWS" -lt 2 ]]; then
	echo "--windows must be at least 2" >&2
	exit 2
fi
if [[ "$SWITCHES" -lt 1 ]]; then
	echo "--switches must be at least 1" >&2
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

SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/bmux-attach-perf.XXXXXX")"
cleanup() {
	rm -rf "$SANDBOX"
}
trap cleanup EXIT

PLAYBOOK="$SANDBOX/tab-switch.dsl"
SAMPLE_JSON_FILE="$SANDBOX/sample.json"

{
	printf '@timeout 30000\n'
	printf '@shell sh\n'
	printf '@viewport cols=120 rows=40\n'
	printf 'new-session\n'
	for ((i = 2; i <= WINDOWS; i += 1)); do
		printf 'send-attach key=%q\n' 'c'
	done
	printf 'send-attach key=%q\n' 'ctrl+h'
	for ((i = 1; i <= SWITCHES; i += 1)); do
		printf 'send-attach key=%q\n' 'ctrl+s'
	done
	printf 'screen\n'
} >"$PLAYBOOK"

export BMUX_ATTACH_PHASE_TIMING=1
if [[ "$SERVICE_TIMING" -eq 1 ]]; then
	export BMUX_SERVICE_PHASE_TIMING=1
fi
if [[ "$IPC_TIMING" -eq 1 ]]; then
	export BMUX_IPC_PHASE_TIMING=1
fi

echo "benchmarking attach tab-switch playbook"
echo "iterations=${ITERATIONS} warmup=${WARMUP} windows=${WINDOWS} switches=${SWITCHES}"

for ((i = 0; i < WARMUP; i += 1)); do
	"$BMUX_BIN" playbook run "$PLAYBOOK" --json >/dev/null 2>&1
done

"$BMUX_PERF_TOOLS_BIN" sample \
	--iterations "$ITERATIONS" \
	--out-json "$SAMPLE_JSON_FILE" \
	-- "$BMUX_BIN" playbook run "$PLAYBOOK" --json

latency_cmd=(
	"$BMUX_PERF_TOOLS_BIN"
	report-latency
	--input "$SAMPLE_JSON_FILE"
)
if [[ -n "$MAX_P99_MS" ]]; then
	latency_cmd+=(--max-p99-ms "$MAX_P99_MS")
fi
"${latency_cmd[@]}"

phase_report() {
	local phase="$1"
	local field="$2"
	local max_p99_ms="$3"
	local filter_key="${4:-}"
	local filter_value="${5:-}"
	local output="$SANDBOX/${phase}.${field}.json"
	local cmd=(
		"$BMUX_PERF_TOOLS_BIN" report-phase-file
		--input "$SAMPLE_JSON_FILE"
		--output "$output"
		--phase "$phase"
		--field "$field"
		--max-p99-ms "$max_p99_ms"
	)
	if [[ -n "$filter_key" ]]; then
		cmd+=(--filter-key "$filter_key" --filter-value "$filter_value")
	fi
	"${cmd[@]}"
}

phase_report attach.plugin_command total_us "$MAX_ATTACH_COMMAND_P99_MS" command_name next-window
phase_report attach.plugin_command before_context_us "$MAX_ATTACH_COMMAND_P99_MS" command_name next-window
phase_report attach.plugin_command run_us "$MAX_ATTACH_COMMAND_P99_MS" command_name next-window
phase_report attach.plugin_command retarget_us "$MAX_RETARGET_P99_MS" command_name next-window
phase_report attach.window_cycle invoke_us "$MAX_ATTACH_COMMAND_P99_MS" command_name next-window
if [[ "$SERVICE_TIMING" -eq 1 ]]; then
	phase_report service.client_invoke total_us "$MAX_ATTACH_COMMAND_P99_MS" operation switch-window
fi
if [[ "$IPC_TIMING" -eq 1 ]]; then
	phase_report ipc.client_request total_us "$MAX_ATTACH_COMMAND_P99_MS" request invoke_service
	phase_report ipc.client_request recv_us "$MAX_ATTACH_COMMAND_P99_MS" request invoke_service
fi
phase_report attach.retarget_context total_us "$MAX_RETARGET_P99_MS"
phase_report attach.retarget_context grant_us "$MAX_RETARGET_P99_MS"
phase_report attach.retarget_context open_us "$MAX_RETARGET_P99_MS"
phase_report attach.retarget_context viewport_us "$MAX_RETARGET_P99_MS"

if [[ -n "$ARTIFACT_JSON" ]]; then
	"$BMUX_PERF_TOOLS_BIN" report-json \
		--input "$SAMPLE_JSON_FILE" \
		--output "$ARTIFACT_JSON"
	echo "artifact_json=$ARTIFACT_JSON"
else
	echo "sample_json=$SAMPLE_JSON_FILE"
fi

echo "attach tab-switch perf check passed"
