#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BMUX_BIN="${BMUX_BIN:-}"
BMUX_PERF_TOOLS_BIN="${BMUX_PERF_TOOLS_BIN:-}"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-attach-tab-switch.sh [options]

Compatibility wrapper for:
  bmux-perf-tools run-benchmark --manifest perf/attach-tab-switch.toml

Common options:
  --iterations N
  --warmup N
  --windows N
  --switches N
  --scenario next-window|prev-window|goto-window|new-window
  --profile normal|diagnostic|ci|stress
  --bmux-bin PATH
  --artifact-json PATH
  --phase-report-dir PATH
  --max-p99-ms N
  --max-attach-command-p99-ms N
  --max-retarget-p99-ms N
  --service-timing, --ipc-timing, --storage-timing are accepted for compatibility and map to diagnostic profile
  -h, --help
USAGE
}

profile_explicit=0
diagnostic_requested=0
args=(--manifest "$ROOT_DIR/perf/attach-tab-switch.toml")
while (($# > 0)); do
	case "$1" in
	--bmux-bin)
		BMUX_BIN="$2"
		shift 2
		;;
	--max-attach-command-p99-ms)
		args+=(--limit "attach_command=$2")
		shift 2
		;;
	--max-retarget-p99-ms)
		args+=(--limit "retarget=$2")
		shift 2
		;;
	--service-timing | --ipc-timing | --storage-timing)
		diagnostic_requested=1
		shift
		;;
	--profile)
		profile_explicit=1
		args+=(--profile "$2")
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		args+=("$1")
		shift
		;;
	esac
done

cd "$ROOT_DIR"
if [[ -z "$BMUX_PERF_TOOLS_BIN" ]]; then
	cargo build -q -p bmux_perf_tools
	BMUX_PERF_TOOLS_BIN="$ROOT_DIR/target/debug/bmux-perf-tools"
fi
if [[ -z "$BMUX_BIN" ]]; then
	cargo build -q -p bmux_cli
	BMUX_BIN="$ROOT_DIR/target/debug/bmux"
fi
if [[ "$diagnostic_requested" -eq 1 && "$profile_explicit" -eq 0 ]]; then
	args+=(--profile diagnostic)
fi

exec "$BMUX_PERF_TOOLS_BIN" run-benchmark --bmux-bin "$BMUX_BIN" "${args[@]}"
