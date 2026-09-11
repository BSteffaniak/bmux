# CLI workflows

The current CLI is server-backed by default. Running `bmux` with no subcommand starts or reuses a server, creates a session when needed, and attaches.

```bash
# Start or inspect the server
bmux server start
bmux server status
bmux server stop

# Create and attach sessions
bmux new-session dev
bmux list-sessions
bmux attach dev

# Work with named workspaces and tabs
bmux workspace new --name project
bmux tab new --name editor
bmux tab list
bmux workspace switch project

# Multi-client collaboration
bmux list-clients
bmux follow <client-uuid>
bmux unfollow

# Remote targets over SSH
bmux connect prod app
bmux connect prod                # picker when multiple sessions
bmux remote list
bmux remote test prod
bmux remote doctor prod --fix
bmux remote init prod --ssh bmux@prod.example.com --set-default
bmux remote install-server prod
bmux remote upgrade prod
bmux --target prod list-sessions
bmux connect prod --reconnect-forever
bmux remote complete targets
bmux remote complete sessions prod

# Streamlined hosted workflow (p2p default, no bmux control-plane required)
bmux setup
bmux host
# Optional runtime instance selection (for parallel local runtimes)
bmux --runtime dev server start --daemon
bmux --runtime dev host

# Native per-user login autostart (launchd, systemd user service, or Task Scheduler)
bmux server autostart install
bmux server autostart status
bmux server autostart print      # side-effect-free declaration output
bmux server autostart uninstall
# Named runtimes receive isolated native service identities:
bmux --runtime dev server autostart install
# Ephemeral sandbox run (fully isolated config/runtime/data/state/logs)
bmux sandbox run -- server status
bmux sandbox run --bmux-bin ./target/debug/bmux --env-mode inherit -- --version
bmux sandbox dev -- server status
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
# Optional control-plane mode for account/share links
bmux setup --mode control-plane
bmux host --mode control-plane
bmux share --name my-host
bmux join bmux://my-host

# Bash/Zsh/Fish completion can call:
# bmux remote complete targets
# bmux remote complete sessions <target>

# Internet-accessible TLS gateway
bmux server gateway --listen 0.0.0.0:7443 --quick
bmux connect tls-prod app

# Or start the TLS gateway automatically with the local server by adding:
# [server.gateway]
# enabled = true
# listen = "0.0.0.0:7443"
# quick = true
# Restart the bmux server after changing gateway settings.

# Reverse-SSH hosted helper (prints public URL in ssh output)
bmux server gateway --listen 127.0.0.1:7443 --quick --host --host-mode ssh
bmux connect https://your-public-url app

# Iroh hosted mode (default for --host)
bmux server gateway --listen 127.0.0.1:7443 --host
# prints: iroh://<endpoint_id>?relay=<url>
bmux connect iroh://<endpoint_id>?relay=<url> app

# Logging
bmux logs path
bmux logs level
bmux logs tail
bmux logs path --json
bmux logs level --json
bmux logs tail --since 15m --lines 200
bmux logs watch --exclude "bmux server listening"
bmux logs watch --profile incident-db
bmux logs profiles list
```

A TLS gateway can also start automatically with the local server:

```toml
[server.gateway]
enabled = true
listen = "0.0.0.0:7443"
quick = true
```

For a production certificate, disable quick mode by omitting `quick` and provide both PEM files:

```toml
[server.gateway]
enabled = true
listen = "0.0.0.0:7443"
cert_file = "/etc/bmux/gateway-cert.pem"
key_file = "/etc/bmux/gateway-key.pem"
```

Restart the bmux server after changing gateway settings. Listening on `0.0.0.0` exposes the gateway to reachable networks; apply appropriate firewall and access controls. Reverse-SSH and Iroh hosting remain explicit `bmux server gateway --host ...` command flows.

`bmux logs watch` uses a ratatui interface and supports Vim-style navigation (`j`/`k`, `g`/`G`, `Ctrl-u`/`Ctrl-d`).

Top-level and grouped command forms are supported in many areas of the CLI.

All list commands with `--json` output a bare JSON array.

Logging defaults:

- file sink is enabled by default
- default level is `info`
- `--verbose` raises level to `debug`
- `--log-level` supports `error|warn|info|debug|trace`

Environment overrides:

- `BMUX_LOG_LEVEL`: effective runtime log level

A native autostart service runs foreground `bmux server start` so the operating system owns supervision; it does not use `--daemon`. Installation is explicit and starts immediately unless `--no-start` is passed. Existing `[server.gateway]` configuration is loaded normally, so an enabled gateway also starts at login.

Imperative installation refuses symlinked, read-only, conflicting, or externally managed declarations. Nix/Home Manager users should define `${pkgs.bmux}/bin/bmux server start` through `systemd.user.services` or `launchd.agents` and let Nix own the declaration; bmux never mutates `bmux.toml`. Linux autostart begins at login and does not enable lingering automatically.

Runtime selection vs sandbox isolation:

- Use `--runtime <name>` to run multiple local bmux runtime instances side-by-side while still using your normal config/data/state roots.
- Use `bmux sandbox ...` when you want a throwaway isolated environment (config/runtime/data/state/logs/home/tmp) for safe local build testing and failure triage.
- `BMUX_LOG_DIR`: explicit log directory
- `BMUX_STATE_DIR`: explicit state directory
- `BMUX_TARGET`: default command target (same behavior as `--target`)

Connection targets can be configured in `bmux.toml`:

```toml
[connections]
hosted_mode = "p2p" # or "control_plane" (hard-fail on control-plane errors)
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

Example shell wiring for target/session completion:

```bash
# Bash helper functions
_bmux_targets() {
  bmux remote complete targets 2>/dev/null
}

_bmux_sessions() {
  local target="$1"
  bmux remote complete sessions "$target" 2>/dev/null
}

# Usage examples:
# _bmux_targets
# _bmux_sessions prod
```

Plugin command ownership policy is optional and declarative:

```toml
[plugins.routing]
conflict_mode = "fail_startup"

[[plugins.routing.required_namespaces]]
namespace = "plugin"

[[plugins.routing.required_paths]]
path = ["recording", "start"]
```

Role policy: `owner` controls session and window mutations plus role changes, `writer` can send attach input, and `observer` is read-only.

Window presentation settings and the tab-bar/sidebar enablement combinations
are documented in [Window presentation plugins](tab-presentations.md).

## Examples

- Prompt showcase (isolated in-process sandbox + attach prompt API):

  ```bash
  cargo run -p bmux_prompt_showcase
  ```

- Plugin-provided prompt showcase (reuses `example.native` prompt sequence):

  ```bash
  cargo run -p bmux_prompt_plugin_showcase
  ```

- Native plugin example:

  ```bash
  cargo build -p bmux_example_native_plugin
  ./scripts/install-example-plugin.sh
  ```

- Minimal hello plugin example:

  ```bash
  cargo build -p bmux_example_hello_plugin
  ```

See the [root README](../README.md) for installation and [TESTING.md](../TESTING.md) for validation. Examples using remote hosts, gateways, service installation, or plugin installation change runtime state; review them before running.
