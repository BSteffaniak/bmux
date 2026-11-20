# bmux_plugin_cli_plugin

Bundled plugin CLI plugin for bmux.

## Overview

Provides the `bmux plugin` command namespace. Implements plugin discovery and
listing, command dispatch to other plugins, and bundled plugin rebuilding from
source. `plugin run` invokes the selected plugin by ID and command name. It
rejects self-invocation rather than re-entering the plugin CLI.

## Commands

- `plugin list` -- discover and display installed plugins with status
- `plugin run <plugin-id> <command> [args...]` -- dispatch a command to a specific plugin
- `plugin rebuild [selector...]` -- recompile bundled plugin crates from source
- `plugin doctor` -- check plugin installation and declarations

Compatibility routes to still-built-in commands are enumerated explicitly in
`src/lib.rs`. The manifest alone cannot create a new core-command route.
