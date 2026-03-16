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

bmux is currently installed by building from source:

```bash
git clone https://github.com/BSteffaniak/bmux.git
cd bmux
cargo build --all-targets
cargo test --all-targets
```

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

Role policy: `owner` controls session and window mutations plus role changes, `writer` can send attach input, and `observer` is read-only.

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
