# bmux

[![Rust](https://github.com/BSteffaniak/bmux/workflows/Rust/badge.svg)](https://github.com/BSteffaniak/bmux/actions)
[![License: MPL 2.0](https://img.shields.io/badge/License-MPL%202.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)

> **⚠️ Work in Progress**: bmux is currently in early development. The core architecture has been established, but most user-facing features are planned but not yet implemented.

**bmux** is a modern, high-performance terminal multiplexer written in Rust. Designed as a powerful alternative to tmux, bmux introduces innovative features like multi-client session sharing with independent views, advanced modal interactions, and extensible plugin architecture.

## ✅ Current Status

- **Development Infrastructure**: Modular architecture with strict code quality standards
- **Local IPC Foundation**: Versioned client/server protocol with cross-platform endpoint abstraction
- **CLI Server Management**: Foreground/daemon server lifecycle commands with graceful stop + fallback
- **Session Command Surface**: Top-level and grouped session commands are available

## 🚀 Key (planned) Features

### Multi-Client Architecture

- **Simultaneous Multi-Client Access**: Multiple clients can connect to the same session simultaneously
- **Independent Views**: Each client can view different panes/windows within the same session (unlike tmux)
- **Client Following**: Optional client synchronization where one client can follow another's view
- **Session Isolation**: Robust session management with proper client isolation

### Advanced Terminal Management

- **Split Panes**: Flexible horizontal and vertical pane splitting with intuitive resizing
- **Multiple Windows**: Organize work across multiple virtual windows within sessions
- **Session Management**: Create, switch, and manage multiple named sessions
- **Cross-Platform**: Native support for Linux, macOS, and Windows

### Performance & Reliability

- **Rust-Powered**: Built with Rust for maximum performance and memory safety
- **Zero-Copy Operations**: Optimized data handling for minimal latency
- **Efficient Rendering**: Smart terminal rendering with minimal screen updates
- **Low Resource Usage**: Designed for efficiency even with many concurrent sessions

### User Experience

- **Modal Interactions**: Vim-inspired interface with Normal mode as default (unlike tmux's prefix-key approach)
- **Fuzzy Search**: Built-in fuzzy finder for sessions, windows, and commands
- **Scrollback History**: Persistent scrollback with search capabilities
- **Customizable**: Extensive configuration options and theming support

### Extensibility

- **Plugin System**: Powerful plugin architecture for extending functionality
- **Scriptable**: Automation support through built-in scripting capabilities
- **Event System**: Hook into session events for custom workflows
- **API Integration**: RESTful API for external tool integration

## 📦 Installation

### Development Build

Since bmux is in early development, installation is currently only available by building from source:

```bash
git clone https://github.com/BSteffaniak/bmux.git
cd bmux
cargo build --all-targets
cargo test --all-targets
```

### Development Tools

For active development, install additional tools:

```bash
# Install development dependencies
cargo install cargo-watch

# Continuous checking during development
cargo watch -x check

# Run clippy for code quality
cargo clippy --all-targets --all-features

# Format code consistently
cargo fmt
```

## 🛠️ Development

## 🧭 Current CLI Workflow

The current CLI supports both top-level and grouped session commands with identical behavior.

```bash
# Start server in foreground (default)
bmux server start

# Start server in background daemon mode
bmux server start --daemon

# Check server status and stop gracefully
bmux server status
bmux server stop

# Create/list/attach/kill sessions (top-level)
bmux new-session dev
bmux list-sessions
bmux list-sessions --json
bmux attach dev
bmux kill-session dev

# Window lifecycle (top-level)
bmux new-window --session dev --name editor
bmux list-windows --session dev
bmux switch-window active --session dev
bmux kill-window active --session dev

# Follow controls (top-level)
bmux follow 550e8400-e29b-41d4-a716-446655440000 --global
bmux unfollow

# Same operations via grouped aliases
bmux session new dev
bmux session list
bmux session list --json
bmux session attach dev
bmux session kill dev
bmux session detach
bmux window new --session dev --name editor
bmux window list --session dev
bmux window switch active --session dev
bmux window kill active --session dev
bmux session follow 550e8400-e29b-41d4-a716-446655440000 --global
bmux session unfollow
```

`list-sessions --json` and `session list --json` output a bare JSON array.

### Server stop behavior

`bmux server stop` uses graceful IPC shutdown first. If that times out, it falls back to PID-based termination.

### Troubleshooting stale PID state

If daemon startup/stop was interrupted, stale PID state can remain under the runtime directory (`$XDG_RUNTIME_DIR/bmux/server.pid` on most Linux setups, or temp-dir based equivalents). The CLI auto-cleans stale PID files when it can prove the process is gone, but you can remove the file manually if needed.

### Building from Source

```bash
git clone https://github.com/BSteffaniak/bmux.git
cd bmux
cargo build --release
```

### Running Tests

```bash
cargo test --all
```

### Development Setup

```bash
# Install development dependencies
cargo install cargo-watch cargo-audit

# Run in development mode with auto-reload
cargo watch -x 'run --bin bmux'
```

### Nix + direnv Setup

If you use Nix, bmux provides a flake-based development shell:

```bash
nix develop
```

To automatically load the shell when entering the repository:

```bash
direnv allow
```

## 📄 License

bmux is licensed under the [Mozilla Public License 2.0](LICENSE).
