#!/usr/bin/env bash
# Run bmux_pty's deterministic simulation (packages/pty/simulator, its own workspace).
#
#   scripts/sim-pty.sh                       100 random runs
#   scripts/sim-pty.sh --runs 1000           more runs
#   scripts/sim-pty.sh --seed 42             runs starting from seed 42
#   scripts/sim-pty.sh --replay 42           replay exactly one run, single-threaded, with logs
#   scripts/sim-pty.sh --regression          every seed in packages/pty/simulator/regression-seeds
#   scripts/sim-pty.sh --verify-determinism  replay every run and fail if it differs
#   scripts/sim-pty.sh --tui                 watch runs in a dashboard (bmux_simvar_tui):
#                                            runs, scheduler, processes and terminals;
#                                            space pauses at a decision, n/N step
#
# Other SIMULATOR_* variables (SIMULATOR_SCHEDULE_STRATEGY, SIMULATOR_PTY_PERSONALITY,
# SIMULATOR_PTY_FAULTS, SIMULATOR_PTY_PID_REUSE, ...) pass through.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
simulator="$root/packages/pty/simulator"

export SIMULATOR_RUNS="${SIMULATOR_RUNS:-100}"
export NO_TUI="${NO_TUI:-1}"
regression=0
tui=

while [ $# -gt 0 ]; do
    case "$1" in
    --runs) SIMULATOR_RUNS="$2"; shift 2 ;;
    --seed) export SIMULATOR_SEED="$2"; shift 2 ;;
    --replay)
        export SIMULATOR_SEED="$2" SIMULATOR_RUNS=1 SIMULATOR_MAX_PARALLEL=1
        export RUST_LOG="${RUST_LOG:-info}"
        shift 2
        ;;
    --verify-determinism) export SIMULATOR_VERIFY_DETERMINISM=1; shift ;;
    --regression) regression=1; shift ;;
    --tui) tui=--tui; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

cd "$simulator"
cargo build --quiet
binary="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/bmux_pty_simulator"

if [ "$regression" = 1 ]; then
    failed=0
    while read -r seed _; do
        case "$seed" in ''|\#*) continue ;; esac
        if ! SIMULATOR_SEED="$seed" SIMULATOR_RUNS=1 SIMULATOR_MAX_PARALLEL=1 \
            SIMULATOR_VERIFY_DETERMINISM=1 "$binary" >/dev/null 2>&1; then
            echo "regression seed $seed failed; replay with: scripts/sim-pty.sh --replay $seed" >&2
            failed=1
        fi
    done < "$simulator/regression-seeds"
    [ "$failed" = 0 ] && echo "all regression seeds passed"
    exit "$failed"
fi

exec "$binary" $tui
