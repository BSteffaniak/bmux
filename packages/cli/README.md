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
- First-class SSH targets (`connect`, `remote`, and global `--target`)

Default launch behavior:

- Running `bmux` with no subcommand now uses the server path only.
- If the server is not running, bmux starts it in daemon mode.
- If no session exists, bmux creates `tab-1` (or the next available `tab-N`) and attaches.

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

## Remote Target Commands

```bash
# connect directly to a target/session
bmux connect prod app

# when session is omitted:
# - 0 sessions: error with next-step command
# - 1 session: auto-select
# - many sessions: interactive picker
bmux connect prod

# remote target utilities
bmux remote list
bmux remote list --json
bmux remote test prod
bmux remote doctor prod --fix
bmux remote init prod --ssh bmux@prod.example.com --set-default
bmux remote install-server prod
bmux remote upgrade prod
bmux connect prod --reconnect-forever
bmux remote complete targets
bmux remote complete sessions prod

# streamlined aliases
bmux setup
bmux host
# optional runtime instance selection (parallel local runtimes)
bmux --runtime dev server start --daemon
bmux --runtime dev host

# sandboxed runs (isolated config/runtime/data/state/logs)
bmux sandbox run -- server status
bmux sandbox dev -- server status
bmux sandbox run --bmux-bin ./target/debug/bmux --env-mode inherit -- attach
bmux sandbox list --limit 10
bmux sandbox status --json
bmux sandbox list --source playbook --limit 10
bmux sandbox inspect --latest
bmux sandbox inspect --latest --source recording-verify
bmux sandbox inspect --latest-failed --tail 120
bmux sandbox tail --latest-failed --tail 120 --json
bmux sandbox open --latest-failed --json
bmux sandbox rerun --latest-failed --bmux-bin ./target/debug/bmux --json
bmux sandbox triage --json
bmux sandbox triage --latest-failed --bundle --bundle-output ./sandbox-artifacts --json
bmux sandbox triage --latest-failed --bundle --bundle-strict-verify --json
bmux sandbox triage --latest-failed --rerun --bmux-bin ./target/debug/bmux
bmux sandbox bundle bmux-sbx-123 --output ./sandbox-artifacts
bmux sandbox bundle bmux-sbx-123 --include-env --verify --json
bmux sandbox verify-bundle ./sandbox-artifacts/bmux-sbx-123-1700000000000 --json
bmux sandbox verify-bundle ./sandbox-artifacts/bmux-sbx-123-1700000000000 --strict --json
bmux sandbox doctor --json
bmux sandbox doctor --fix --dry-run --json
bmux sandbox cleanup --dry-run --json
bmux sandbox clean --dry-run --json
bmux sandbox cleanup --all-status --source playbook --older-than 0
bmux sandbox cleanup --source recording-verify --older-than 600
# opt-in control-plane mode (hard-fails if control-plane operations fail)
bmux setup --mode control-plane
bmux host --mode control-plane
bmux join bmux://my-host
bmux hosts
bmux auth login
bmux share --name my-host
bmux unshare my-host

# run an internet-accessible TLS gateway
bmux server gateway --listen 0.0.0.0:7443 --quick

# or force reverse-SSH hosting helper instead of iroh
bmux server gateway --listen 127.0.0.1:7443 --quick --host --host-mode ssh

# iroh hosted mode is default when --host is enabled
bmux server gateway --listen 127.0.0.1:7443 --host
# prints iroh://<endpoint_id>?relay=<url>
bmux connect iroh://<endpoint_id>?relay=<url> app

# target any normal command remotely
bmux --target prod list-sessions
bmux --target prod attach app
```

Shell completion snippets:

```bash
# Bash: target completion for `bmux connect <TAB>`
_bmux_complete_targets() {
  COMPREPLY=( $(compgen -W "$(bmux remote complete targets 2>/dev/null)" -- "${COMP_WORDS[COMP_CWORD]}") )
}
complete -F _bmux_complete_targets bmux

# Bash: session completion helper
_bmux_complete_sessions() {
  local target="$1"
  bmux remote complete sessions "$target" 2>/dev/null
}

# Zsh helper examples
_bmux_targets() { bmux remote complete targets 2>/dev/null }
_bmux_sessions() { bmux remote complete sessions "$1" 2>/dev/null }

# Fish helper examples
function __bmux_targets
  bmux remote complete targets 2>/dev/null
end
```

Target precedence for command routing:

1. `--target`
2. `BMUX_TARGET`
3. `[connections].default_target`
4. local target

Example target config:

```toml
[connections]
default_target = "local"

[connections.targets.prod]
transport = "ssh"
host = "prod.example.com"
user = "bmux"
port = 22
identity_file = "~/.ssh/id_ed25519"
known_hosts_file = "~/.ssh/known_hosts"
strict_host_key_checking = true
jump = "ops@bastion.example.com"
remote_bmux_path = "bmux"
connect_timeout_ms = 8000
default_session = "main"

[connections.targets.tls-prod]
transport = "tls"
host = "gateway.example.com"
port = 7443
server_name = "gateway.example.com"
ca_file = "~/.config/bmux/gateway-ca.pem"
```

TLS targets support standard command routing via `--target` (for commands that use normal client connections), plus `bmux connect` and remote utility commands.

Hosted URLs are accepted directly by connect (for example: `bmux connect https://abc123.example.net app`).

For long-lived unstable links, `bmux connect` supports `--reconnect-forever`.

## Logging Commands

```bash
# show effective log file path
bmux logs path
bmux logs path --json

# show effective runtime level
bmux logs level
bmux logs level --json

# show recent lines and keep following
bmux logs tail

# show a fixed slice and exit
bmux logs tail --lines 200 --no-follow

# show entries newer than relative time
bmux logs tail --since 15m

# interactive watch with seed filters
bmux logs watch --exclude "bmux server listening"
bmux logs watch --include-i "warn|error"

# use named profile state
bmux logs watch --profile incident-db

# manage saved profiles
bmux logs profiles list
bmux logs profiles show incident-db
bmux logs profiles rename incident-db incident-prod
bmux logs profiles delete incident-prod
```

Logging behavior:

- bmux writes logs to file by default.
- default level is `info`.
- `--verbose` raises level to `debug`.
- `--log-level error|warn|info|debug|trace` overrides both defaults and `--verbose`.
- `logs path --json` and `logs level --json` return object output.
- `logs tail --since <duration>` filters by RFC3339 timestamps in log lines (`s`, `m`, `h`, `d` units).
- `logs watch` provides a live interactive viewer with non-destructive include/exclude filters.
- `logs watch` filter seed flags: `--include`, `--include-i`, `--exclude`, `--exclude-i`.
- `logs watch` saves filter/session state across runs (default global profile `default`).
- `logs watch --profile <name>` scopes saved state to a named profile for a specific workflow.
- `logs profiles list|show|delete|rename` manages saved watch profiles.
- `logs watch` uses a ratatui interface for scalable log tooling.
- `logs watch` keys: `a` add include, `x` add exclude, `t` toggle rule, `i` toggle per-filter case mode, `d` delete rule, `c` clear rules, `/` quick substring filter, `p` pause, `q` quit.
- Vim-style navigation: `j`/`k` move, `g`/`G` top/bottom, `Ctrl-u`/`Ctrl-d` half-page, `PageUp`/`PageDown` full-page.

Log/state directory conventions:

- Linux
  - state: `$XDG_STATE_HOME/bmux` (fallback: `~/.local/state/bmux`)
  - logs: `<state>/logs` (override with `BMUX_LOG_DIR`)
- macOS
  - state: `~/Library/Application Support/bmux/State`
  - logs: `~/Library/Logs/bmux`
- Windows
  - state: `%LOCALAPPDATA%\\bmux\\State`
  - logs: `%LOCALAPPDATA%\\bmux\\Logs`

Environment overrides:

- `BMUX_STATE_DIR`: override state root
- `BMUX_LOG_DIR`: override log directory
- `BMUX_LOG_LEVEL`: set runtime log level (`error|warn|info|debug|trace`)

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

## Bundled Plugins

Plugins bundled next to the `bmux` executable are enabled by default.

Use config opt-out when you want to disable specific bundled plugins:

```toml
[plugins]
disabled = ["bmux.windows", "bmux.permissions", "bmux.clipboard"]
```

You can still explicitly enable additional non-bundled plugins:

```toml
[plugins]
enabled = ["example.native"]
```

Optional routing policy can enforce required plugin command ownership at startup
without hardcoding plugin IDs in core runtime:

```toml
[plugins.routing]
conflict_mode = "fail_startup"

[[plugins.routing.required_namespaces]]
namespace = "plugin"

[[plugins.routing.required_paths]]
path = ["playbook", "run"]
```

`required_namespaces` and `required_paths` support optional `owner = "plugin.id"`
when you want a specific plugin to own the claim.

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
