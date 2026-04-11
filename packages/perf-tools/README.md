# bmux perf tools

Small helper utilities used by `scripts/perf-plugin-command-latency.sh` and
`scripts/perf-plugin-runtime-matrix.sh`.

The crate exists to keep perf scripts Python-free while preserving stable
script output and SLO checks.

## Commands

- `sample` - execute a command repeatedly and write sample/fault JSON
- `report-latency` - print latency summary and enforce p95/p99/avg thresholds
- `report-faults` - print runtime fault counters and enforce fault thresholds
- `discover-run-candidate` - discover a successful `bmux plugin run` pair
