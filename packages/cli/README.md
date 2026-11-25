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
- Remote access to paired hosts (`connect`, `remote`, and global `--target`)

Default launch behavior:

- Running `bmux` with no subcommand now uses the server path only.
- If the server is not running, bmux starts it in daemon mode.
- If no session exists, bmux creates `tab-1` (or the next available `tab-N`) and attaches.

The native autostart service runs `bmux server start` in the foreground so
launchd, systemd, or Task Scheduler owns supervision and restart behavior; it
deliberately does not use `--daemon`. `install` starts the service immediately
unless `--no-start` is passed. Rerun `install` after moving or upgrading a
non-stable executable path. If remote access is enabled (`bmux remote enable`),
it starts normally with the managed server; there is no separate remote access
setting for autostart.

On Linux, autostart targets the user manager's `default.target` and therefore
starts at login. bmux does not enable lingering; users who explicitly need
startup before login can separately configure `loginctl enable-linger`.

`autostart install` safely refuses declarations owned by Nix, Home Manager,
another tool, or the user. `autostart print` is side-effect-free and can be used
by declarative tooling.

### Nix/Home Manager

Declarative users should have Nix own the native service and should not run
imperative `autostart install`. For Home Manager on Linux:

```nix
systemd.user.services.bmux-server = {
  Unit.Description = "bmux server";
  Service = {
    Type = "simple";
    ExecStart = "${pkgs.bmux}/bin/bmux server start";
    Restart = "on-failure";
    RestartSec = 1;
  };
  Install.WantedBy = [ "default.target" ];
};
```

For Home Manager/nix-darwin on macOS:

```nix
launchd.agents.bmux-server = {
  enable = true;
  config = {
    ProgramArguments = [ "${pkgs.bmux}/bin/bmux" "server" "start" ];
    RunAtLoad = true;
    KeepAlive = {
      Crashed = true;
      SuccessfulExit = false;
    };
    ProcessType = "Background";
  };
};
```

These immutable package paths update and roll back with the owning Nix
generation. A read-only Nix-generated `bmux.toml` is supported because autostart
only reads normal bmux configuration and never modifies it.

## Server Commands

```bash
# foreground (default)
bmux server start

# background daemon mode
# The native service manager supervises foreground `bmux server start`.
bmux server autostart install
bmux server autostart status
bmux server autostart status --json
bmux server autostart print
bmux server autostart uninstall

# `--runtime work` creates an isolated runtime-qualified native service.
bmux --runtime work server autostart install

# Existing manual daemon startup remains available:
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

## Remote Access Commands

Remote access is provided by the `bmux.connections` plugin. A host pairs each
device once; the device then attaches over Iroh (QUIC) without running a server
of its own. See [docs/remote-pairing.md](../../docs/remote-pairing.md).

```bash
# host: enable remote access once, then pair a device
bmux remote enable
bmux remote pair                         # prints a one-time link, then approves
bmux remote pair --role view-only --name laptop

# connecting device: paste the link, compare the phrase with the host
bmux connect --pair
bmux connect --pair --name desk --device-name work-mac

# connecting device: attach to the saved host
bmux connect desk
bmux connect desk app
bmux attach --target desk app

# target any normal command at a saved host
bmux --target desk list-sessions

# connecting device: saved hosts
bmux remote hosts
bmux remote rename desk desktop
bmux remote forget desktop

# host: devices and live connections
bmux remote list
bmux remote status
bmux remote role laptop view-only
bmux remote revoke laptop
bmux remote disable
```

Target precedence for command routing:

1. `--target`
2. `BMUX_TARGET`
3. `[connections].default_target`
4. local target

A target is `local` or the name of a saved host (`bmux remote hosts`). Bare
`bmux` with no subcommand attaches locally unless `--target` is given. Targets
in `[connections.targets]` with an `ssh`, `tls` or `iroh` transport are no
longer dialed; remove them and pair the host instead.

## Runtime and Sandbox Commands

```bash
# optional runtime instance selection (parallel local runtimes)
bmux --runtime dev server start --daemon

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
```

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
bmux new-tab --session dev --name editor
bmux list-tabs --session dev
bmux list-tabs --session dev --json
bmux switch-tab active --session dev
bmux kill-tab active --session dev
bmux kill-tab active --session dev --force-local
bmux kill-all-tabs --session dev
bmux kill-all-tabs --session dev --force-local

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

When `--session` is omitted, window commands use the currently attached session
context.

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
disabled = ["bmux.tabs", "bmux.permissions", "bmux.clipboard"]
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

`required_namespaces` and `required_paths` support optional `owner =
"plugin.id"`
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
- `bmux list-tabs --json`
- `bmux window list --json`

Output format is a bare JSON array.

## Troubleshooting

- If daemon state is stale after an interrupted start/stop, rerun `bmux server status` and `bmux server stop` first; CLI includes stale PID cleanup logic.
- If a stale PID file still exists, remove `server.pid` from bmux runtime dir and restart server.
