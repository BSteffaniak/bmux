# bmux_plugin_cli_plugin

Bundled plugin CLI plugin for bmux.

## Overview

Provides the `bmux plugin` command namespace. Implements plugin discovery and
listing, command dispatch to other plugins, and bundled plugin rebuilding from
source. Also proxies several core CLI commands (logs, diagnostics, recording,
playbook, config, server) under the plugin namespace for a consistent user
experience.

## Commands

- `plugin list` -- discover and display installed plugins with status
- `plugin run <plugin-id> <command> [args...]` -- dispatch a command to a specific plugin
- `plugin rebuild [selector...]` -- recompile bundled plugin crates from source

### Proxied Commands

- `logs-path`, `logs-level`, `logs-tail`, `logs-watch` -- log management
- `keymap-doctor`, `terminal-doctor` -- diagnostic tools
- `recording-*` -- recording management and export
- `playbook-*` -- headless scripted execution
- `config-*` -- configuration inspection
- `server-*` -- server lifecycle management
