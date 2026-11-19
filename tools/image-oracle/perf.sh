#!/usr/bin/env bash
# Measure only the already-built executable, never Cargo/rustc. One process per sample.
set -euo pipefail
bin=${1:?usage: perf.sh /path/to/image-oracle reference|candidate [repetitions]}
backend=${2:?specify backend}
case $(uname -s) in
  Darwin) time_args=(-l) ;; # maximum resident set size in bytes
  Linux) time_args=(-v) ;; # maximum resident set size in KiB (GNU time)
  *) echo 'unsupported peak RSS platform' >&2; exit 2 ;;
esac
for ((i=0; i<${3:-3}; i++)); do
  for workload in flat noise; do
    /usr/bin/time "${time_args[@]}" "$bin" perf "$backend" "$workload"
  done
done
