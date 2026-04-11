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
bmux sandbox list --status all --limit 20 --json
bmux sandbox inspect bmux-sbx-123 --tail 120
bmux sandbox doctor --json
bmux sandbox cleanup --dry-run --json
```
