# Plugin Performance and Usability Gates

This document defines the non-negotiable "no regression" bar for plugin architecture changes.

## Goal

- Keep or improve plugin performance while cleaning up architecture.
- Keep or improve plugin usability while removing hardcoded and duplicated surfaces.

## Release Gates

Any change touching plugin runtime, plugin SDK, command routing, or bundled plugins must pass these gates.

### 1) Build and correctness

- `cargo nextest run --no-fail-fast`
- `cargo clippy --all-targets -- -D warnings`
- `cargo machete --with-metadata`

### 2) CLI/runtime safety checks

For changes under `packages/cli/**`:

- `cargo test -p bmux_cli`
- `cargo check -p bmux_cli`
- `./scripts/smoke-pty-runtime.sh`

### 3) Plugin build and activation

For changes under `plugins/**`:

- `bmux plugin rebuild`

### 4) End-to-end command latency SLO

- `./scripts/perf-plugin-command-latency.sh --iterations 20 --warmup 5 --max-p95-ms 250 --max-p99-ms 350`
- `./scripts/perf-plugin-runtime-matrix.sh --iterations 20 --warmup 5`

For scale-focused scenarios, generate synthetic manifests during sampling:

- `./scripts/perf-plugin-runtime-matrix.sh --iterations 20 --warmup 5 --scale-profile small`
- `./scripts/perf-plugin-runtime-matrix.sh --iterations 20 --warmup 5 --scale-profile medium`
- `./scripts/perf-plugin-runtime-matrix.sh --iterations 20 --warmup 5 --scale-profile large`

Scale profile defaults:

- `small` -> 40 synthetic plugins
- `medium` -> 120 synthetic plugins
- `large` -> 300 synthetic plugins

Use `--scale-plugin-count <N>` when you need an explicit custom size.

Optional steady-state thresholds are available when you want to gate post-startup behavior:

- `--max-steady-p95-ms`
- `--max-steady-p99-ms`
- `--max-steady-avg-ms`

Perf scripts are Python-free and use the workspace helper binary (`bmux-perf-tools`).
The scripts auto-build the helper on first use.

For CI artifact archiving, use:

- `./scripts/perf-plugin-command-latency.sh ... --artifact-json <path>.json`
- `./scripts/perf-plugin-runtime-matrix.sh ... --artifact-dir <dir>`

For baseline comparisons (informational):

- `./scripts/perf-plugin-artifact-compare.sh --candidate-dir <dir> --baseline-dir docs/perf-baselines/runtime-matrix`

For repeated-run variance analysis (informational):

- `./scripts/perf-plugin-variance.sh --runs 3 --iterations 8 --warmup 2`

For variance policy evaluation (warn-only by default):

- `./scripts/perf-variance-policy-check.sh --report-dir <compare-report-dir> --policy-file docs/perf-baselines/variance-policy.json --mode warn`

To trial soft-fail behavior locally:

- `./scripts/perf-variance-policy-check.sh --report-dir <compare-report-dir> --policy-file docs/perf-baselines/variance-policy.json --mode soft-fail`

CI soft-fail trial toggle:

- Set repository variable `BMUX_PERF_VARIANCE_ENFORCE=1`.

### 5) Plugin runtime command matrix SLO

`scripts/perf-plugin-runtime-matrix.sh` enforces per-scenario p95/p99 SLOs:

- `plugin list --json`: p95 \<= 250ms, p99 \<= 350ms
- `plugin doctor --json`: p95 \<= 350ms, p99 \<= 500ms
- `plugin rebuild --list --json`: p95 \<= 550ms, p99 \<= 750ms
- `plugin run missing.plugin-id no-op`: p95 \<= 350ms, p99 \<= 550ms (expected non-zero path)
- `plugin run <discovered-plugin> <discovered-command>`: p95 \<= 450ms, p99 \<= 650ms when an environment-supported command exists

The matrix also applies matching steady-state p95/p99 thresholds (excluding first measured sample) for each scenario.

Use `--cold` to run the same matrix without warmup samples.

## Usability Acceptance Matrix

The following workflows must remain functional and keep equivalent UX quality:

- `bmux plugin list --json` returns discovered + enabled metadata.
- `bmux plugin run <plugin> <command> [args...]` reports actionable errors for not-found/not-enabled/denied cases.
- plugin command ownership conflicts fail startup with clear conflict details.
- required namespace/path routing claims fail startup when unmet.
- plugin aliases and nested paths resolve deterministically (longest-prefix semantics).
- fallback behavior remains intact when optional plugins are unavailable.

## Architecture Guardrails

- Core architecture must remain domain-agnostic for windows/permissions semantics.
- Command ownership remains policy-driven, not hardcoded to specific plugin IDs.
- Changes should prefer generic host interfaces and plugin capabilities over core special cases.

## Change Policy

- Any optimization must preserve externally visible behavior.
- Any cleanup must keep latency flat or better for hot paths.
- If a cleanup would trade off UX or latency, split it and land only the safe portion.

## References

- Plugin ops index: `docs/plugin-ops.md`
- Plugin triage playbook: `docs/plugin-triage-playbook.md`
- Perf troubleshooting and baseline updates: `docs/plugin-perf-troubleshooting.md`
