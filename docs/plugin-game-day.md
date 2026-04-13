# Plugin Game Day

Use this game day to validate end-to-end operator workflows for common plugin incidents.

## Scenario 1: Missing Plugin Invocation

Goal: verify actionable not-found guidance.

```bmux-cli
bmux plugin run missing.plugin-id no-op
```

Expected result:

- non-zero exit
- output includes the missing plugin id and guidance to run `bmux plugin list --json`

Artifacts to capture:

- command output (stdout/stderr)
- `bmux plugin list --json`

## Scenario 2: Policy Denial Guidance Contract

Goal: ensure denied operations preserve policy-specific `Hint`/`Next` guidance.

```bash
cargo test -p bmux_plugin_cli_plugin run_cmd::tests::format_plugin_command_run_error_adds_policy_hint_when_denied
```

Expected result:

- test passes
- denial messaging contains active policy hint and authorized principal next step

Artifacts to capture:

- test output

## Scenario 3: Perf Threshold Regression Drill

Goal: validate regression handling path and troubleshooting sequence.

```bash
./scripts/perf-plugin-command-latency.sh \
  --iterations 1 \
  --warmup 0 \
  --max-p95-ms 1 \
  --max-p99-ms 1 \
  -- plugin list --json
```

Expected result:

- non-zero exit due to intentionally impossible thresholds
- output includes threshold violation details

Follow-up commands:

```bash
./scripts/perf-plugin-command-latency.sh --iterations 20 --warmup 5 --max-p95-ms 250 --max-p99-ms 350 --artifact-json /tmp/plugin-command-latency.json
./scripts/perf-plugin-artifact-compare.sh --candidate-dir /tmp --baseline-dir docs/perf-baselines --warn-regression-ms 20
```

Artifacts to capture:

- failing perf output
- candidate artifact JSON
- compare output

## Quick Runner

You can run all scenarios with:

```bash
./scripts/plugin-game-day.sh
```
