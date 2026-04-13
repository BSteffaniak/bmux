# Plugin Triage Playbook

Use this playbook when plugin behavior is failing, unclear, or regressing.

## Quickstart

Run these in order and keep outputs as investigation artifacts.

```bmux-cli
bmux plugin list --json
bmux plugin doctor --json --strict
bmux plugin rebuild --list --json
```

If command execution is failing for a specific plugin command:

```bmux-cli
bmux plugin run <plugin-id> --help
bmux plugin run <plugin-id> <command> --help
```

## Fast Decision Tree

1. `plugin list` does not show expected plugin
   - Check search paths and manifests.
   - Confirm plugin id is correct.
2. `plugin doctor` reports errors
   - Fix errors first; warnings in strict mode are still treated as failures.
3. `plugin rebuild --list` does not include expected crate
   - Verify selector/id/short-name and workspace plugin crate mapping.
4. `plugin run` fails with not-found/command-not-found
   - Use suggested `Next:` guidance and `--help` command listing.
5. `plugin run` fails with policy denial
   - Verify active policy provider and principal authorization.

## Useful Focused Commands

```bmux-cli
bmux plugin list --enabled-only --json
bmux plugin list --capability bmux.commands --json
bmux plugin doctor --severity error --json
bmux plugin doctor --code manifest --json
bmux plugin doctor --summary-only
```

## What to Attach in a Bug Report

- `bmux plugin list --json`
- `bmux plugin doctor --json --strict`
- `bmux plugin rebuild --list --json`
- failing `bmux plugin run ...` command + stderr output
- commit SHA and platform info

If performance is part of the bug, also attach:

- plugin command latency artifact JSON
- runtime matrix artifact directory
- runtime matrix scale artifact directory (if scale-sensitive)

See `docs/plugin-perf-troubleshooting.md` for perf-specific triage.
