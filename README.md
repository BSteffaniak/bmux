# bmux

[![Rust](https://github.com/BSteffaniak/bmux/workflows/Rust/badge.svg)](https://github.com/BSteffaniak/bmux/actions)
[![License: MPL 2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)

> **Work in Progress**: bmux is in active development. The architecture is moving quickly, and the current CLI surface is already usable while the broader experience continues to take shape.

**bmux** is a modern terminal multiplexer written in Rust, built for flexible multi-client workflows, modal interaction, and deep customization. bmux is plugin-driven by design, making extensibility a first-class part of the product rather than an afterthought.

## Why bmux

- Multi-client sessions with independent views
- Modal, keyboard-driven interaction
- Plugin-driven extensibility and customization
- A modern Rust implementation focused on performance and reliability
- A server-backed CLI built for reusable session workflows

## Extensibility

Extensibility in bmux is built in, not bolted on.

Plugins are not an afterthought in bmux - they are part of the architecture and part of how bmux works. That makes bmux flexible to adapt, easier to customize, and better suited for workflows that do not fit a one-size-fits-all terminal multiplexer.

## Current Status

Today, bmux includes a working server-backed CLI, session and window management workflows, multi-client foundations, diagnostics, and plugin-driven extensibility. It is still early, but it is no longer just a skeleton or roadmap.

## Installation

### Source build

```bash
git clone https://github.com/BSteffaniak/bmux.git
cd bmux
cargo build --all-targets
cargo test --all-targets
```

### npm

```bash
npm install -g bmux
# or
npm install -g @bmux/cli
```

### packages.bmux.dev channels

Stable is the default channel.

```bash
curl -fsSL https://packages.bmux.dev/install | sh
curl -fsSL "https://packages.bmux.dev/install?channel=nightly" | sh
```

APT and RPM repository roots:

- `https://packages.bmux.dev/stable/apt`
- `https://packages.bmux.dev/stable/rpm`
- `https://packages.bmux.dev/nightly/apt`
- `https://packages.bmux.dev/nightly/rpm`

## Current CLI Workflow

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

# Work with windows
bmux window new --session dev --name editor
bmux window list --session dev

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
# Ephemeral sandbox run (fully isolated config/runtime/data/state/logs)
bmux sandbox run -- server status
bmux sandbox run --bmux-bin ./target/debug/bmux --env-mode inherit -- --version
bmux sandbox cleanup --dry-run --json
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

## Development

Useful commands:

```bash
cargo check
cargo test --all
cargo clippy --all-targets --all-features
cargo fmt
```

For active development:

```bash
cargo install cargo-watch
cargo watch -x check
bmux plugin rebuild
```

Build specific plugins by bundled id, short name, or crate name:

```bash
bmux plugin rebuild bmux.windows
bmux plugin rebuild windows permissions
bmux plugin rebuild bmux_windows_plugin --release
```

### Nix + direnv

If you use Nix, bmux provides a flake-based development shell:

```bash
nix develop
```

To automatically load the shell when entering the repository:

```bash
direnv allow
```

## License

bmux is licensed under the [Mozilla Public License 2.0](LICENSE).
