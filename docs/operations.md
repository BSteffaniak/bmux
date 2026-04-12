# Operations Guide

Operational commands for maintaining bmux runtimes and diagnosing issues.

## Runtime Health

```bmux-cli
bmux server status
bmux doctor
bmux perf status --json
```

## Recording Hygiene

```bmux-cli
bmux recording status --json
bmux recording prune --older-than 14 --json
```

## Playbook Maintenance

```bmux-cli
bmux playbook cleanup --dry-run --json
```

## Sandbox Maintenance

```bmux-cli
bmux sandbox run -- server status
bmux sandbox run --bmux-bin ./target/debug/bmux --env-mode clean -- server start
bmux sandbox dev -- attach
bmux sandbox list --status all --limit 20 --json
bmux sandbox status --json
bmux sandbox list --source recording-verify --limit 20 --json
bmux sandbox inspect --latest
bmux sandbox inspect --latest --source playbook
bmux sandbox inspect --latest-failed --tail 120
bmux sandbox bundle bmux-sbx-123 --output ./sandbox-artifacts --json
bmux sandbox doctor --json
bmux sandbox cleanup --dry-run --json
bmux sandbox clean --dry-run --json
bmux sandbox cleanup --source playbook --older-than 600 --json
bmux sandbox cleanup --all-status --source playbook --older-than 0 --json
bmux sandbox rebuild-index --json
```

Cleanup output includes per-entry `reason` for observability (`would_remove`,
`removed`, `running`, `recent`, `not_failed`, `missing_manifest`,
`source_mismatch`, `delete_failed`) plus top-level reason counters.

Sandbox list/inspect/cleanup/status JSON responses now include a `reconcile`
object so you can see when auto-heal paths rebuilt or pruned index state.

`bmux sandbox cleanup` uses `[sandbox.cleanup]` defaults from `bmux.toml` when
flags are omitted. CLI flags always win.

```toml
[sandbox.cleanup]
failed_only = false
older_than_secs = 300
source = "all" # sandbox_cli | playbook | recording_verify | all
```

## Runtime Namespaces vs Sandboxes

- Use `bmux --runtime <name> ...` for parallel named local runtimes that still use your normal user config/data/state roots.
- Use `bmux sandbox ...` for fully isolated ephemeral runs intended for local build validation and failure reproduction.
