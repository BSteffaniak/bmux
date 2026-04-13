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
bmux sandbox tail --latest-failed --tail 120 --json
bmux sandbox open --latest-failed --json
bmux sandbox rerun --latest-failed --bmux-bin ./target/debug/bmux --json
bmux sandbox triage --json
bmux sandbox triage --latest-failed --source playbook --tail 120 --json
bmux sandbox triage --latest-failed --rerun --bmux-bin ./target/debug/bmux
bmux sandbox bundle bmux-sbx-123 --output ./sandbox-artifacts --json
bmux sandbox bundle bmux-sbx-123 --include-env --include-index-state --include-doctor --json
bmux sandbox bundle bmux-sbx-123 --include-env --verify --json
bmux sandbox verify-bundle ./sandbox-artifacts/bmux-sbx-123-1700000000000 --json
bmux sandbox doctor --json
bmux sandbox doctor --fix --dry-run --json
bmux sandbox doctor --fix --json
bmux sandbox cleanup --dry-run --json
bmux sandbox clean --dry-run --json
bmux sandbox cleanup --source playbook --older-than 600 --json
bmux sandbox cleanup --all-status --source playbook --older-than 0 --json
bmux sandbox rebuild-index --json
```

## Sandbox Triage Flow

Use this sequence when a sandboxed run fails and you need fast reproduction data.

```bmux-cli
# 1) Confirm current sandbox health and reconcile activity
bmux sandbox status --json

# 2) Focus the latest failed run (optionally scoped by source)
bmux sandbox tail --latest-failed --tail 120 --json
bmux sandbox open --latest-failed --json
bmux sandbox tail --latest-failed --source playbook --tail 120 --json

# 3) Re-run with the same command from manifest metadata
bmux sandbox rerun --latest-failed --bmux-bin ./target/debug/bmux --json

# 4) Preview and apply repair if state drift is detected
bmux sandbox doctor --fix --dry-run --json
bmux sandbox doctor --fix --json
```

### Bundle/Triage Quick Decision Table

| Goal                                     | Command                                          | Default Verify Behavior                                                      | Strict Extras Behavior                                           |
| ---------------------------------------- | ------------------------------------------------ | ---------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| Create a shareable bundle only           | `bmux sandbox bundle <id> --json`                | No verify step                                                               | N/A                                                              |
| Create + verify in one command           | `bmux sandbox bundle <id> --verify --json`       | Runs metadata verify and fails on required drift                             | Add `--strict` via `verify-bundle` for extra artifact failures   |
| Verify an existing bundle                | `bmux sandbox verify-bundle <bundle-dir> --json` | Reports unexpected extras but does not fail for them                         | Use `--strict` to fail on unexpected extras                      |
| Triage and auto-package failure evidence | `bmux sandbox triage <id> --bundle --json`       | Auto-runs verify on created bundle and triage exits non-zero if verify fails | Add `--bundle-strict-verify` to fail triage on unexpected extras |

Strictness rule of thumb:

- Missing/changed expected artifacts (`exists`, `bytes`, `file_count`, `sha256`) fail verification in both strict and non-strict modes.
- Unexpected extra artifacts are informational in non-strict mode and blocking in strict mode.

Troubleshooting notes:

- `latest_log: null` in JSON means no log file exists yet in that sandbox `logs/` dir.
- `* target required` errors mean no explicit target or selector was provided; use `<id|path>`, `--latest`, or `--latest-failed`.
- `sandbox target '<id>' is ambiguous` means your prefix matches multiple runs; use a full id from `bmux sandbox list --limit 20`.
- `sandbox target not found ... did you mean: ...` gives nearby id hints when the target is close but not exact.
- `sandbox manifest '<id>' has no command to rerun` means the manifest command array is empty; inspect manifest integrity or rerun from a different target.
- If stale `running`/lock metadata appears, run `bmux sandbox doctor --fix --dry-run --json` first, then apply with `--fix`.

### Bundle Verify Failures

If `bmux sandbox bundle ... --verify` or `bmux sandbox verify-bundle ...` returns drift:

- Re-run verification in JSON mode and inspect `issues[]` for exact `path` + `field` mismatches.
- `field=exists` usually means a bundled artifact was moved/deleted after creation.
- `field=bytes` or `field=file_count` usually means files inside the bundle changed post-creation.
- `field=sha256` means content-level drift was detected even if file counts/sizes look unchanged.
- `unexpected_artifacts[]` lists files present in the bundle dir but not declared in `artifact_metadata`.
- `version_check.ok=false` means the bundle manifest/schema version is incompatible with this bmux binary.
- Regenerate a fresh bundle from the original sandbox when drift is expected (`bmux sandbox bundle <id> --verify --json`).
- For archived bundles, treat verification drift as a chain-of-custody signal and avoid mutating the directory in place.

### Chain-of-Custody Notes

- Bundles record per-artifact hashes in `artifact_metadata[].sha256` for both files and directories.
- Directory hashes are deterministic over the directory tree contents, so reorder-only filesystem effects do not change the digest.
- Legacy bundles without `sha256` metadata still verify for existence/size/count checks; rebuild bundles to get full hash guarantees.
- If you intend to archive or share evidence, run verify immediately before handoff and include the full JSON report.

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

## Plugin Ops References

- Plugin operations index: `docs/plugin-ops.md`
- Triage and failure playbook: `docs/plugin-triage-playbook.md`
- Perf gate troubleshooting and baseline guidance: `docs/plugin-perf-troubleshooting.md`
- Operator game day scenarios: `docs/plugin-game-day.md`
