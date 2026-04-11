#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_SCRIPT="$ROOT_DIR/scripts/perf-plugin-command-latency.sh"

BMUX_BIN="${BMUX_BIN:-}"
ITERATIONS="${ITERATIONS:-30}"
WARMUP="${WARMUP:-5}"
COLD_MODE="0"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-plugin-runtime-matrix.sh [options]

Runs plugin usability/runtime latency checks across key command paths.

Options:
  --bmux-bin PATH     Use an explicit bmux executable
  --iterations N      Measured iterations per scenario (default: 30)
  --warmup N          Warmup iterations per scenario (default: 5)
  --cold              Run without warmup (sets warmup to 0)
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

run_case_allow_nonzero() {
	local title="$1"
	local max_p95_ms="$2"
	local max_p99_ms="$3"
	shift 3
	local args=("$@")

	echo
	echo "=== ${title} ==="
	local cmd=(
		"$BASE_SCRIPT"
		--allow-nonzero
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

find_happy_plugin_run_args() {
	local bmux_cmd=(cargo run -q -p bmux_cli --bin bmux --)
	if [[ -n "$BMUX_BIN" ]]; then
		bmux_cmd=("$BMUX_BIN")
	fi

	local list_json
	if ! list_json=$("${bmux_cmd[@]}" plugin list --json 2>/dev/null); then
		return 1
	fi

	local candidates
	if ! candidates=$(
		python3 - "$list_json" <<'PY'
import json
import sys

payload = json.loads(sys.argv[1])
for entry in payload:
    plugin_id = entry.get("id")
    if plugin_id == "bmux.plugin_cli":
        continue
    for command in entry.get("commands", []):
        print(f"{plugin_id}\t{command}")
PY
	); then
		return 1
	fi

	while IFS=$'\t' read -r plugin_id command; do
		if [[ -z "$plugin_id" || -z "$command" ]]; then
			continue
		fi
		if "${bmux_cmd[@]}" plugin run "$plugin_id" "$command" >/dev/null 2>&1; then
			printf '%s\n' "$plugin_id" "$command"
			return 0
		fi
	done <<<"$candidates"

	return 1
}

parse_args "$@"
require_number "$ITERATIONS" "--iterations"
require_number "$WARMUP" "--warmup"

if [[ "$COLD_MODE" == "1" ]]; then
	WARMUP=0
fi

if [[ ! -x "$BASE_SCRIPT" ]]; then
	echo "missing executable script: $BASE_SCRIPT" >&2
	exit 2
fi

run_case "plugin list json" 250 350 plugin list --json
run_case "plugin doctor json" 350 500 plugin doctor --json
run_case "plugin rebuild list json" 550 750 plugin rebuild --list --json
run_case_allow_nonzero "plugin run missing plugin" 350 550 plugin run missing.plugin-id no-op

if happy_args=$(find_happy_plugin_run_args); then
	mapfile -t parts <<<"$happy_args"
	run_case "plugin run discovered command" 450 650 plugin run "${parts[0]}" "${parts[1]}"
else
	echo
	echo "=== plugin run discovered command ==="
	echo "Skipping: no successful plugin run candidate discovered in this environment"
fi

echo
echo "plugin runtime matrix perf checks passed"
