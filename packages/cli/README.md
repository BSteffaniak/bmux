# bmux_cli

Command-line interface for bmux terminal multiplexer.

## Overview

`bmux_cli` provides:

- Local server lifecycle commands (`start`, `status`, `stop`)
- Session lifecycle commands (new/list/attach/detach/kill)
- Alias-compatible command forms (top-level and grouped)
- Runtime/terminal diagnostics (`keymap doctor`, `terminal doctor`)

## Server Commands

```bash
# foreground (default)
bmux server start

# background daemon mode
bmux server start --daemon

# status and graceful shutdown
bmux server status
bmux server stop
```

Shutdown behavior:

- `bmux server stop` tries graceful IPC shutdown first.
- If graceful shutdown times out, CLI falls back to PID-based termination.

## Session Commands

Top-level and grouped forms are exact aliases.

```bash
# top-level
bmux new-session dev
bmux list-sessions
bmux list-sessions --json
bmux attach dev
bmux detach
bmux kill-session dev

# grouped aliases
bmux session new dev
bmux session list
bmux session list --json
bmux session attach dev
bmux session detach
bmux session kill dev
```

Session target values for `attach`/`kill` support both:

- session name
- session UUID

## JSON Output

`--json` is supported on session list commands:

- `bmux list-sessions --json`
- `bmux session list --json`

Output format is a bare JSON array.

## Troubleshooting

- If daemon state is stale after an interrupted start/stop, rerun `bmux server status` and `bmux server stop` first; CLI includes stale PID cleanup logic.
- If a stale PID file still exists, remove `server.pid` from bmux runtime dir and restart server.
