# Plugin Ops Index

Operator entrypoint for plugin usability, diagnosis, and performance workflows.

## Start Here

Run this sequence first for most plugin issues:

```bmux-cli
bmux plugin list --json
bmux plugin doctor --json --strict
bmux plugin rebuild --list --json
```

Then use command help for a specific plugin command surface:

```bmux-cli
bmux plugin run <plugin-id> --help
bmux plugin run <plugin-id> <command> --help
```

## Navigation Map

- Triage playbook (failure diagnosis and report artifacts):
  - `docs/plugin-triage-playbook.md`
- Performance troubleshooting (gates, baseline compare, update policy):
  - `docs/plugin-perf-troubleshooting.md`
- Perf/usability gates and thresholds:
  - `docs/plugin-performance-usability-gates.md`
- Baseline artifacts and refresh commands:
  - `docs/perf-baselines/README.md`
- Game day scenarios:
  - `docs/plugin-game-day.md`

## If-Then Quick Guide

- If plugin not found in list:
  - Verify plugin id and search roots, then rerun `bmux plugin list --json`.
- If doctor reports issues:
  - Fix `error` findings first; in strict mode warnings are treated as failures.
- If rebuild selector fails:
  - Use `bmux plugin rebuild --list` and select by id, short name, or crate name.
- If run fails with not-found command:
  - Use `bmux plugin run <plugin-id> --help` to inspect known commands.
- If run fails with policy denial:
  - Verify policy provider state and authorized principal.
- If perf compare reports WARN:
  - Re-run with warmup/iterations and compare startup vs steady-state before refreshing baselines.

## High-Value Command Recipes

```bmux-cli
bmux plugin list --enabled-only --json
bmux plugin list --capability bmux.commands --json
bmux plugin list --compact

bmux plugin doctor --summary-only
bmux plugin doctor --severity error --json
bmux plugin doctor --code manifest --json
```
