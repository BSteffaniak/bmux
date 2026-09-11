# bmux

[![Build and Test](https://github.com/BSteffaniak/bmux/actions/workflows/ci.yml/badge.svg)](https://github.com/BSteffaniak/bmux/actions/workflows/ci.yml)

**A Rust terminal multiplexer and reusable terminal UI framework.**

bmux combines server-backed terminal sessions with independent client views, modal interaction, typed plugin services, and composable themes. The UI framework can also be used without the multiplexer.

> **Early alpha.** APIs, configuration, and terminal compatibility are evolving. Source installation is the supported path described here. Package-release workflows exist, but npm/package-server availability must not be inferred from those workflows.

## Capabilities

- Persistent server-backed sessions and multi-client views.
- Plugin-owned session, window, workspace, permission, and command behavior.
- A domain-neutral TUI framework with layout, painting, input, selection, damage tracking, and reusable controls.
- Additive theme stacks, mode-aware overlays, and plugin-owned interactive components.
- Kitty, Sixel, and iTerm2 image handling, subject to host-terminal support and implemented protocol operations.
- Remote connections, recording/export, and runtime diagnostics.

## Installation

Install Git, stable Rust, and a native build toolchain. macOS and Linux are primary terminal environments; the repository also contains Windows support and platform-specific release configuration. A release target is not a guarantee of identical behavior on every terminal/OS combination.

```sh
git clone https://github.com/BSteffaniak/bmux.git
cd bmux
cargo build --locked --release -p bmux_cli --bin bmux
./target/release/bmux --help
./target/release/bmux
```

On Windows, use `target\release\bmux.exe`. Use the built binary's full path, or add its directory to `PATH` before using the examples below.

## Current CLI Workflow

Running without a subcommand starts or reuses a local server, creates a session when needed, and attaches.

```sh
bmux new-session dev
bmux list-sessions
bmux attach dev
```

Inside an attached session, `bmux detach` disconnects the client without intentionally terminating the session. Read [configuration profiles](docs/config-profiles.md) for modal and tmux-compatible interaction rather than assuming another multiplexer’s keybindings.

## Architecture

| Layer                                            | Responsibility                                                                       |
| ------------------------------------------------ | ------------------------------------------------------------------------------------ |
| [`bmux_tui`](packages/tui)                       | Domain-neutral geometry, layout, paint, scenes, and terminal presentation primitives |
| [`bmux_tui_components`](packages/tui-components) | Reusable controls with caller-owned state                                            |
| [`bmux_tui_runtime`](packages/tui-runtime)       | Scheduling, events, and presentation lifecycle                                       |
| [Plugin APIs and services](docs/plugins.md)      | Typed product behavior and extension contracts                                       |
| [Theme plugin](plugins/theme-plugin)             | Theme selection and composition                                                      |
| [Image handling](docs/images.md)                 | Protocol interception, storage, and presentation                                     |

Read the [TUI framework guide](docs/tui-framework.md) for reusable APIs and ownership boundaries. Sessions surviving client disconnects do not imply that arbitrary foreground processes survive a machine restart.

## Documentation

- [CLI workflows and advanced examples](docs/cli-workflows.md)
- [Concepts](docs/concepts.md) and [configuration profiles](docs/config-profiles.md)
- [Plugins](docs/plugins.md) and [window presentation](docs/tab-presentations.md)
- [Images and compression](docs/images.md)
- [Operations](docs/operations.md) and [testing](TESTING.md)

Remote gateways and login startup are opt-in operations. Review their network exposure and supervision behavior before enabling them. Do not copy gateway examples into an internet-facing deployment without appropriate access controls.

## Development

```sh
cargo fmt
cargo check -p bmux_cli
cargo test -p bmux_cli
```

For code changes, follow [AGENTS.md](AGENTS.md): warning-free clippy, the nextest suite, dependency checks, and relevant PTY/compatibility tests. Docs-only changes use link, Markdown, and documentation-snippet checks. Report the platform and terminal alongside non-sensitive reproductions in [GitHub Issues](https://github.com/BSteffaniak/bmux/issues).

Native plugins are trusted code, not sandboxed extensions. Report security-sensitive issues privately to [bradensteffaniak@gmail.com](mailto:bradensteffaniak@gmail.com), without credentials or private recordings. No response-time guarantee is implied.

## License

[Mozilla Public License 2.0](LICENSE). Bundled fonts retain their [upstream licenses](packages/fonts/licenses).
