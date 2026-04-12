# Perf Baselines

This directory stores optional JSON baseline artifacts for plugin perf comparisons.

- `plugin-command-latency.json` should match the output schema from `bmux-perf-tools report-json`.
- `runtime-matrix/*.json` files should match per-scenario artifacts from `scripts/perf-plugin-runtime-matrix.sh --artifact-dir ...`.

CI uses these baselines for informational comparisons. Missing files are skipped.
