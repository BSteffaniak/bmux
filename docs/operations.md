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
