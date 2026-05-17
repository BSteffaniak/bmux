# BMUX perf audit

This directory contains repeatable perf scenarios for the attach/render path.

Run the quick suite:

```sh
./scripts/perf-audit.sh --quick
```

Run every scenario:

```sh
./scripts/perf-audit.sh --full
```

Artifacts are written to `target/perf-audit/<timestamp>/` and include:

- `playbook.json` — raw playbook result with per-step render summaries
- `perf.json` — `bmux recording analyze --perf --json` output when telemetry exists
- `metrics.json` — normalized metrics used for budget checks
- `violations.json` — failed budget checks
- `run.log` — command stderr/log output

The suite intentionally combines two signal sources:

1. **Playbook render summaries** from `@render-trace true`, which are deterministic
   in sandbox runs and include rows/cells/frame bytes plus terminal graphics
   transmit/place/delete counters.
2. **Recording perf telemetry** from `bmux recording analyze --perf --json`, which
   is best-effort for playbook recordings and richer for live/manual recordings
   that include `bmux.perf` custom events.

Budgets live in `tests/perf/budgets/*.json` and are matched by scenario name.
Use `ignore_actions` to exclude warmup/setup steps such as `new-session` from
render budget aggregation.

For local config/plugin validation against an already-running BMUX server, use:

```sh
./scripts/perf-audit.sh --target-server --quick
```

For CI, prefer the default sandbox mode for determinism.
