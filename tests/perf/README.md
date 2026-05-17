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
- `recommendations.json` — actionable findings derived from budgets, real perf telemetry, and render summaries
- `run.log` — command stderr/log output

The suite uses `@driver real-attach` scenarios so production `bmux.perf`
recording telemetry is the source of truth for real performance. The driver runs
the normal attach runtime against a headless terminal adapter instead of relying
only on playbook-side render reconstruction. `scripts/perf-audit.sh` runs playbooks with
`BMUX_PLAYBOOK_PERF_RECORDING_LEVEL=trace` by default, and playbook recordings
explicitly include the `custom` event kind so real attach/server telemetry is
available whenever the scenario drives those production paths.

Playbook render summaries from `@render-trace true` remain useful deterministic
fallback signals for CI budgets. They include rows/cells/frame bytes plus
terminal graphics transmit/place/delete counters, but they should not replace
real attach telemetry for diagnosing runtime performance.

Budgets live in `tests/perf/budgets/*.json` and are matched by scenario name.
Use `ignore_actions` to exclude warmup/setup steps such as `new-session` from
render budget aggregation.

For local config/plugin validation against an already-running BMUX server, use:

```sh
./scripts/perf-audit.sh --target-server --quick
```

Use `--require-perf-events` for real-runtime audits that should fail when a
scenario does not produce `bmux.perf` events. This is the preferred guard for
live/nightly suites once scenarios drive the real attach runtime.

For CI, prefer the default sandbox mode for deterministic render-summary
budgets, and add real-runtime jobs with `--require-perf-events` where a TTY/PTY
attach harness is available.
