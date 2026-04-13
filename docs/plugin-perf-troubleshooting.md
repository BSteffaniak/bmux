# Plugin Perf Troubleshooting

Use this guide when plugin perf gates fail or CI compare output reports regressions.

## 1) Reproduce Locally With Artifacts

```bash
./scripts/perf-plugin-command-latency.sh \
  --iterations 20 \
  --warmup 5 \
  --max-p95-ms 250 \
  --max-p99-ms 350 \
  --artifact-json /tmp/plugin-command-latency.json

./scripts/perf-plugin-runtime-matrix.sh \
  --iterations 20 \
  --warmup 5 \
  --artifact-dir /tmp/plugin-runtime-matrix
```

For scale behavior:

```bash
./scripts/perf-plugin-runtime-matrix.sh \
  --iterations 8 \
  --warmup 2 \
  --scale-profile medium \
  --artifact-dir /tmp/plugin-runtime-matrix-scale
```

## 2) Distinguish Startup Noise vs Steady-State Regression

- Compare `startup_ms` to steady-state metrics (`latency_steady_ms`).
- A large startup spike with healthy steady-state can indicate cold cache/process startup noise.
- Re-run with warmup and multiple iterations before treating as a true regression.

## 3) Compare Against Baselines

```bash
./scripts/perf-plugin-artifact-compare.sh \
  --candidate-dir /tmp/plugin-runtime-matrix \
  --baseline-dir docs/perf-baselines/runtime-matrix \
  --warn-regression-ms 20
```

Use scale baseline compare for scale scenarios:

```bash
./scripts/perf-plugin-artifact-compare.sh \
  --candidate-dir /tmp/plugin-runtime-matrix-scale \
  --baseline-dir docs/perf-baselines/runtime-matrix-scale \
  --warn-regression-ms 30
```

## 4) Baseline Update Policy

Refresh baselines only when behavior is intentionally changed or improved.

- Do not refresh baselines to mask accidental regressions.
- Keep baseline refresh in a dedicated PR when possible.
- Include before/after compare output in PR description.

Baseline refresh commands are documented in `docs/perf-baselines/README.md`.

## 5) What to Include in Perf PRs

- command lines used (`iterations`, `warmup`, scale profile/count)
- artifact file paths
- compare output (`OK`/`IMPROVED`/`WARN` lines)
- whether failures were startup-only or steady-state
