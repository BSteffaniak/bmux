# Command Cookbook

Task-oriented command recipes you can copy, adapt, and automate.

## Session Lifecycle

```bmux-cli
bmux new-session dev
bmux attach dev
bmux list-sessions --json
```

## Remote Target Workflow

```bmux-cli
bmux remote list --json
bmux remote test prod
bmux connect prod app
```

## Hosted Flow

```bmux-cli
bmux setup --mode p2p
bmux host --status
bmux hosts
```

## Logging and Diagnostics

```bmux-cli
bmux logs path --json
bmux logs level --json
bmux logs tail --since 15m --lines 200 --no-follow
```

## Ephemeral Sandbox Workflow

```bmux-cli
# Run a bmux command in a clean isolated sandbox
bmux sandbox run -- server status

# Test a specific local bmux build in isolation
bmux sandbox run --bmux-bin ./target/debug/bmux -- attach

# Keep artifacts/logs for debugging a failing sandbox run
bmux sandbox run --keep --bmux-bin ./target/debug/bmux -- server start

# Override to inherit parent environment instead of clean mode
bmux sandbox run --env-mode inherit -- server status

# Clean up orphaned sandbox directories
bmux sandbox cleanup --dry-run --json
```
