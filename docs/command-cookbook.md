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

# Dev shortcut (keep artifacts on failures, prefers ./target/debug/bmux when present)
bmux sandbox dev -- server status

# Test a specific local bmux build in isolation
bmux sandbox run --bmux-bin ./target/debug/bmux -- attach

# Keep artifacts/logs for debugging a failing sandbox run
bmux sandbox run --keep --bmux-bin ./target/debug/bmux -- server start

# Override to inherit parent environment instead of clean mode
bmux sandbox run --env-mode inherit -- server status

# More strict mode with minimal inherited environment
bmux sandbox run --env-mode hermetic -- server status

# Kill long-running sandbox command after 45 seconds
bmux sandbox run --timeout 45 -- server start

# Print resolved sandbox env map for reproducibility
bmux sandbox run --print-env -- server status

# Discover recent sandboxes and inspect one
bmux sandbox list --limit 10
bmux sandbox list --source playbook --limit 10
bmux sandbox inspect bmux-sbx-123 --tail 120
bmux sandbox inspect --latest
bmux sandbox inspect --latest-failed --tail 120

# Health checks for sandbox runtime
bmux sandbox doctor --json

# Bundle diagnostics and logs for sharing
bmux sandbox bundle bmux-sbx-123 --output ./sandbox-artifacts
bmux sandbox bundle bmux-sbx-123 --json

# Clean up orphaned sandbox directories
bmux sandbox cleanup --dry-run --json
bmux sandbox cleanup --failed-only --older-than 600
bmux sandbox cleanup --source recording-verify --older-than 600
```

## Sandbox Daily Loop

```bmux-cli
# 1) Validate your local build in isolation
bmux sandbox dev --bmux-bin ./target/debug/bmux -- server status

# 2) Re-check the most recent failed run
bmux sandbox inspect --latest-failed --tail 200

# 3) Package logs + repro metadata for teammates
bmux sandbox bundle bmux-sbx-123 --output ./sandbox-artifacts
```
