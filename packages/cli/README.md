# bmux_cli

Command-line interface for bmux terminal multiplexer.

## Overview

`bmux_cli` provides:

- Local server lifecycle commands (`start`, `status`, `stop`)
- Session lifecycle commands (new/list/attach/detach/kill)
- Client listing command (list-clients / session clients)
- Session role controls (permissions/grant/revoke)
- Window lifecycle commands (new/list/switch/kill)
- Client follow controls (follow/unfollow)
- Alias-compatible command forms (top-level and grouped)
- Runtime/terminal diagnostics (`keymap doctor`, `terminal doctor`)

Default launch behavior:

- Running `bmux` with no subcommand now uses the server path only.
- If the server is not running, bmux starts it in daemon mode.
- If no session exists, bmux creates `session-1` (or the next available `session-N`) and attaches.

## Server Commands

```bash
# foreground (default)
bmux server start

# background daemon mode
bmux server start --daemon

# status and graceful shutdown
bmux server status
bmux server status --json
bmux server whoami-principal
bmux server save
bmux server restore --dry-run
bmux server restore --yes
bmux server stop
```

Shutdown behavior:

- `bmux server stop` tries graceful IPC shutdown first.
- If graceful shutdown times out, CLI falls back to PID-based termination.
- `bmux server restore --yes` replaces the current in-memory server state with the persisted snapshot.
- `--force-local` kill bypass is allowed only when your profile principal matches the server owner principal (`bmux server whoami-principal`).

## Session Commands

Top-level and grouped forms are exact aliases.

```bash
# top-level
bmux new-session dev
bmux list-sessions
bmux list-clients
bmux list-clients --json
bmux permissions --session dev
bmux permissions --session dev --json
bmux permissions --session dev --watch
bmux grant --session dev --client 550e8400-e29b-41d4-a716-446655440000 --role writer
bmux revoke --session dev --client 550e8400-e29b-41d4-a716-446655440000
bmux list-sessions --json
bmux attach dev
bmux attach --follow 550e8400-e29b-41d4-a716-446655440000 --global
bmux detach
bmux kill-session dev
bmux kill-session dev --force-local
bmux kill-all-sessions
bmux kill-all-sessions --force-local

# grouped aliases
bmux session new dev
bmux session list
bmux session clients
bmux session clients --json
bmux session permissions --session dev
bmux session permissions --session dev --json
bmux session permissions --session dev --watch
bmux session grant --session dev --client 550e8400-e29b-41d4-a716-446655440000 --role writer
bmux session revoke --session dev --client 550e8400-e29b-41d4-a716-446655440000
bmux session list --json
bmux session attach dev
bmux session attach --follow 550e8400-e29b-41d4-a716-446655440000 --global
bmux session detach
bmux session kill dev
bmux session kill dev --force-local
bmux session kill-all
bmux session kill-all --force-local
```

Session target values for `attach`/`kill` support both:

- session name
- session UUID

Attach also supports follow mode:

- `bmux attach --follow <client-uuid>`
- `bmux attach --follow <client-uuid> --global`

Attach UI defaults (user-overridable via keybindings):

- `Ctrl-A d`: detach
- `Ctrl-A [`: enter scrollback mode for the focused pane
- scrollback mode: arrows or `h/j/k/l` move the cursor, `v` starts selection, `y` copies selection, `Enter` copies selection and exits (or just exits if nothing is selected), `PageUp/PageDown` page, `Ctrl-Y/Ctrl-E` line-scroll the viewport, `g/G` top/bottom, `Ctrl-A ]` or `Esc` cancel selection / exit
- `Ctrl-T`: enter window mode
- window mode: `H/L` previous/next session (wrap), `h/l` previous/next window (wrap), `1..9` jump to index, `n` new window, `x` close active window, `Esc`/`Enter` exit window mode

Prefix timeout behavior is configurable under `[keybindings]`:

- omit both `timeout_profile` and `timeout_ms` to keep prefix mode active indefinitely
- set `timeout_profile = "fast" | "traditional" | "slow"` for named timed behavior
- override built-in profile values with `[keybindings.timeout_profiles]`
- set `timeout_ms` for an exact override; it takes precedence over `timeout_profile`

```toml
[keybindings]
prefix = "ctrl+a"
timeout_profile = "traditional"

[keybindings.timeout_profiles]
traditional = 450
```

Sample timeout sections:

```toml
# Default modal-style prefix: stays active until the next key
[keybindings]
prefix = "ctrl+a"
```

```toml
# Named timed behavior with user-overridden built-in profile values
[keybindings]
prefix = "ctrl+a"
timeout_profile = "traditional"

[keybindings.timeout_profiles]
fast = 180
traditional = 450
slow = 900
```

```toml
# Exact millisecond override wins over timeout_profile
[keybindings]
prefix = "ctrl+a"
timeout_profile = "traditional"
timeout_ms = 275
```

## Window Commands

Top-level and grouped forms are exact aliases.

```bash
# top-level
bmux new-window --session dev --name editor
bmux list-windows --session dev
bmux list-windows --session dev --json
bmux switch-window active --session dev
bmux kill-window active --session dev
bmux kill-window active --session dev --force-local
bmux kill-all-windows --session dev
bmux kill-all-windows --session dev --force-local

# grouped aliases
bmux window new --session dev --name editor
bmux window list --session dev
bmux window list --session dev --json
bmux window switch active --session dev
bmux window kill active --session dev
bmux window kill active --session dev --force-local
bmux window kill-all --session dev
bmux window kill-all --session dev --force-local
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

## Permission Commands

Top-level and grouped forms are exact aliases.

```bash
# top-level
bmux permissions --session dev
bmux permissions --session dev --json
bmux permissions --session dev --watch
bmux grant --session dev --client 550e8400-e29b-41d4-a716-446655440000 --role writer
bmux revoke --session dev --client 550e8400-e29b-41d4-a716-446655440000

# grouped aliases
bmux session permissions --session dev
bmux session permissions --session dev --json
bmux session permissions --session dev --watch
bmux session grant --session dev --client 550e8400-e29b-41d4-a716-446655440000 --role writer
bmux session revoke --session dev --client 550e8400-e29b-41d4-a716-446655440000
```

Role policy:

- `owner`: can mutate session/window state and manage roles
- `writer`: can send attach input only
- `observer`: read-only attach

## JSON Output

`--json` is supported on list commands:

- `bmux list-sessions --json`
- `bmux session list --json`
- `bmux list-clients --json`
- `bmux session clients --json`
- `bmux permissions --session <name|uuid> --json`
- `bmux session permissions --session <name|uuid> --json`
- `bmux list-windows --json`
- `bmux window list --json`

Output format is a bare JSON array.

## Troubleshooting

- If daemon state is stale after an interrupted start/stop, rerun `bmux server status` and `bmux server stop` first; CLI includes stale PID cleanup logic.
- If a stale PID file still exists, remove `server.pid` from bmux runtime dir and restart server.
