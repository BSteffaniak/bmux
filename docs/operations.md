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
bmux sandbox list --source recording-verify --limit 20 --json
bmux sandbox inspect --latest
bmux sandbox inspect --latest-failed --tail 120
bmux sandbox bundle bmux-sbx-123 --output ./sandbox-artifacts --json
bmux sandbox doctor --json
bmux sandbox cleanup --dry-run --json
bmux sandbox cleanup --source playbook --older-than 600 --json
```

## Runtime Namespaces vs Sandboxes

- Use `bmux --runtime <name> ...` for parallel named local runtimes that still use your normal user config/data/state roots.
- Use `bmux sandbox ...` for fully isolated ephemeral runs intended for local build validation and failure reproduction.
