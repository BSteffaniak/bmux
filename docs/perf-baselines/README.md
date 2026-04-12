# Perf Baselines

This directory stores JSON baseline artifacts for plugin perf comparisons.

- `plugin-command-latency.json` should match the output schema from `bmux-perf-tools report-json`.
- `runtime-matrix/*.json` files should match per-scenario artifacts from `scripts/perf-plugin-runtime-matrix.sh --artifact-dir ...`.

CI uses these baselines for informational comparisons.

## Baseline policy

- Baseline comparisons are non-blocking and emit `OK`/`IMPROVED`/`WARN` status lines.
- Hard regressions are still enforced by existing runtime perf thresholds in the perf scripts.
- Refresh baselines only when a change intentionally improves or shifts steady-state behavior.

## Refresh commands

Run from repository root:

```bash
./scripts/perf-plugin-command-latency.sh \
  --iterations 3 \
  --warmup 1 \
  --max-p95-ms 10000 \
  --max-p99-ms 15000 \
  --max-avg-ms 10000 \
  --max-steady-p95-ms 10000 \
  --max-steady-p99-ms 15000 \
  --max-steady-avg-ms 10000 \
  --artifact-json docs/perf-baselines/plugin-command-latency.json \
  -- plugin list --json

./scripts/perf-plugin-runtime-matrix.sh \
  --iterations 2 \
  --warmup 0 \
  --cold \
  --artifact-dir docs/perf-baselines/runtime-matrix

./scripts/perf-plugin-runtime-matrix.sh \
  --iterations 1 \
  --warmup 0 \
  --cold \
  --scale-profile medium \
  --artifact-dir docs/perf-baselines/runtime-matrix-scale
```
