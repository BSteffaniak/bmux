# bmux_cli

Command-line interface for bmux terminal multiplexer.

## Overview

`bmux_cli` provides:

- Local server lifecycle commands (`start`, `status`, `stop`)
- Session lifecycle commands (new/list/attach/detach/kill)
- Client listing command (list-clients / session clients)
- Window lifecycle commands (new/list/switch/kill)
- Client follow controls (follow/unfollow)
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
bmux list-clients
bmux list-clients --json
bmux list-sessions --json
bmux attach dev
bmux detach
bmux kill-session dev

# grouped aliases
bmux session new dev
bmux session list
bmux session clients
bmux session clients --json
bmux session list --json
bmux session attach dev
bmux session detach
bmux session kill dev
```

Session target values for `attach`/`kill` support both:

- session name
- session UUID

## Window Commands

Top-level and grouped forms are exact aliases.

```bash
# top-level
bmux new-window --session dev --name editor
bmux list-windows --session dev
bmux list-windows --session dev --json
bmux switch-window active --session dev
bmux kill-window active --session dev

# grouped aliases
bmux window new --session dev --name editor
bmux window list --session dev
bmux window list --session dev --json
bmux window switch active --session dev
bmux window kill active --session dev
```

Window target values for `switch`/`kill` support:

- window name
- window UUID
- `active`

When `--session` is omitted, window commands use the currently attached session context.

## Follow Commands

Top-level and grouped forms are exact aliases.

```bash
# top-level
bmux list-clients
bmux follow 550e8400-e29b-41d4-a716-446655440000
bmux follow 550e8400-e29b-41d4-a716-446655440000 --global
bmux unfollow

# grouped aliases
bmux session clients
bmux session follow 550e8400-e29b-41d4-a716-446655440000
bmux session follow 550e8400-e29b-41d4-a716-446655440000 --global
bmux session unfollow
```

`follow` target must be a client UUID.

## JSON Output

`--json` is supported on list commands:

- `bmux list-sessions --json`
- `bmux session list --json`
- `bmux list-clients --json`
- `bmux session clients --json`
- `bmux list-windows --json`
- `bmux window list --json`

Output format is a bare JSON array.

## Troubleshooting

- If daemon state is stale after an interrupted start/stop, rerun `bmux server status` and `bmux server stop` first; CLI includes stale PID cleanup logic.
- If a stale PID file still exists, remove `server.pid` from bmux runtime dir and restart server.
