#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ITERATIONS="${ITERATIONS:-60}"
WARMUP="${WARMUP:-10}"
MAX_P99_MS="${MAX_P99_MS:-}"
MAX_P95_MS="${MAX_P95_MS:-}"
MAX_AVG_MS="${MAX_AVG_MS:-}"
MAX_RUNTIME_RETRIES="${MAX_RUNTIME_RETRIES:-}"
MAX_RUNTIME_RESPAWNS="${MAX_RUNTIME_RESPAWNS:-}"
MAX_RUNTIME_TIMEOUTS="${MAX_RUNTIME_TIMEOUTS:-}"
ALLOW_NONZERO="0"

BMUX_BIN="${BMUX_BIN:-}"
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
  --max-runtime-retries N   Fail if runtime retry warnings exceed N
  --max-runtime-respawns N  Fail if runtime respawn warnings exceed N
  --max-runtime-timeouts N  Fail if runtime timeout warnings exceed N
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
if [[ -n "$MAX_RUNTIME_RETRIES" ]]; then
	require_number "$MAX_RUNTIME_RETRIES" "--max-runtime-retries"
fi
if [[ -n "$MAX_RUNTIME_RESPAWNS" ]]; then
	require_number "$MAX_RUNTIME_RESPAWNS" "--max-runtime-respawns"
fi
if [[ -n "$MAX_RUNTIME_TIMEOUTS" ]]; then
	require_number "$MAX_RUNTIME_TIMEOUTS" "--max-runtime-timeouts"
fi

if [[ "${#TARGET_ARGS[@]}" -eq 0 ]]; then
	echo "expected bmux command args after --" >&2
	usage
	exit 2
fi

cd "$ROOT_DIR"

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

echo "benchmarking: bmux ${TARGET_ARGS[*]}"
echo "iterations=${ITERATIONS} warmup=${WARMUP}"

for ((i = 0; i < WARMUP; i += 1)); do
	if [[ "$ALLOW_NONZERO" == "1" ]]; then
		"$BMUX_BIN" "${TARGET_ARGS[@]}" >/dev/null 2>&1 || true
	else
		"$BMUX_BIN" "${TARGET_ARGS[@]}" >/dev/null 2>&1
	fi
done

SAMPLES_FILE="$SANDBOX/samples_ms.txt"
RUNTIME_METRICS_FILE="$SANDBOX/runtime_metrics.txt"
BMUX_PERF_ALLOW_NONZERO="$ALLOW_NONZERO" python3 - "$ITERATIONS" "$SAMPLES_FILE" "$RUNTIME_METRICS_FILE" -- "$BMUX_BIN" -- "${TARGET_ARGS[@]}" <<'PY'
import pathlib
import subprocess
import sys
import time

iterations = int(sys.argv[1])
samples_path = pathlib.Path(sys.argv[2])
metrics_path = pathlib.Path(sys.argv[3])

argv = sys.argv[4:]
first_sep = argv.index("--")
second_sep = argv.index("--", first_sep + 1)
cmd = argv[first_sep + 1:second_sep]
target = argv[second_sep + 1:]
allow_nonzero = bool(int(__import__("os").environ.get("BMUX_PERF_ALLOW_NONZERO", "0")))

samples = []
runtime_retries = 0
runtime_respawns = 0
runtime_timeouts = 0
for _ in range(iterations):
    start_ns = time.perf_counter_ns()
    completed = subprocess.run([*cmd, *target], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    end_ns = time.perf_counter_ns()
    if completed.returncode != 0 and not allow_nonzero:
        print(f"command exited with non-zero status: {completed.returncode}", file=sys.stderr)
        sys.exit(completed.returncode)
    stderr_text = completed.stderr or ""
    runtime_retries += stderr_text.count("persistent process worker write failed; recycling worker")
    runtime_retries += stderr_text.count("persistent process worker read failed; recycling worker")
    runtime_respawns += stderr_text.count("persistent process worker exited; respawning")
    runtime_timeouts += stderr_text.count("persistent process worker read timed out; recycling worker")
    runtime_timeouts += stderr_text.count("process runtime one-shot invocation timed out")
    samples.append((end_ns - start_ns) / 1_000_000)

samples_path.write_text("\n".join(f"{value:.3f}" for value in samples), encoding="utf-8")
metrics_path.write_text(
    "\n".join(
        [
            f"retries={runtime_retries}",
            f"respawns={runtime_respawns}",
            f"timeouts={runtime_timeouts}",
        ]
    ),
    encoding="utf-8",
)
PY

python3 - "$SAMPLES_FILE" "$MAX_P99_MS" "$MAX_P95_MS" "$MAX_AVG_MS" <<'PY'
import math
import pathlib
import sys

samples = [float(line) for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines() if line]
max_p99 = int(sys.argv[2]) if sys.argv[2] else None
max_p95 = int(sys.argv[3]) if sys.argv[3] else None
max_avg = int(sys.argv[4]) if sys.argv[4] else None

if not samples:
    print("no samples collected", file=sys.stderr)
    sys.exit(2)

samples.sort()

def percentile_nearest_rank(values, p):
    rank = max(1, math.ceil((p / 100.0) * len(values)))
    return values[rank - 1]

p50 = percentile_nearest_rank(samples, 50)
p95 = percentile_nearest_rank(samples, 95)
p99 = percentile_nearest_rank(samples, 99)
avg = sum(samples) / len(samples)
min_v = samples[0]
max_v = samples[-1]

print(f"latency_ms min={min_v:.3f} p50={p50:.3f} p95={p95:.3f} p99={p99:.3f} avg={avg:.3f} max={max_v:.3f}")

violations = []
if max_p99 is not None and p99 > max_p99:
    violations.append(f"p99 {p99:.3f} > {max_p99}")
if max_p95 is not None and p95 > max_p95:
    violations.append(f"p95 {p95:.3f} > {max_p95}")
if max_avg is not None and avg > max_avg:
    violations.append(f"avg {avg:.3f} > {max_avg}")

if violations:
    print("SLO check failed: " + "; ".join(violations), file=sys.stderr)
    sys.exit(1)
PY

python3 - "$RUNTIME_METRICS_FILE" "$MAX_RUNTIME_RETRIES" "$MAX_RUNTIME_RESPAWNS" "$MAX_RUNTIME_TIMEOUTS" <<'PY'
import pathlib
import sys

lines = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
metrics = {}
for line in lines:
    if not line or "=" not in line:
        continue
    key, value = line.split("=", 1)
    metrics[key] = int(value)

max_retries = int(sys.argv[2]) if sys.argv[2] else None
max_respawns = int(sys.argv[3]) if sys.argv[3] else None
max_timeouts = int(sys.argv[4]) if sys.argv[4] else None

retries = metrics.get("retries", 0)
respawns = metrics.get("respawns", 0)
timeouts = metrics.get("timeouts", 0)

print(
    f"runtime_faults retries={retries} respawns={respawns} timeouts={timeouts}"
)

violations = []
if max_retries is not None and retries > max_retries:
    violations.append(f"retries {retries} > {max_retries}")
if max_respawns is not None and respawns > max_respawns:
    violations.append(f"respawns {respawns} > {max_respawns}")
if max_timeouts is not None and timeouts > max_timeouts:
    violations.append(f"timeouts {timeouts} > {max_timeouts}")

if violations:
    print("runtime fault check failed: " + "; ".join(violations), file=sys.stderr)
    sys.exit(1)
PY

echo "perf check passed"
