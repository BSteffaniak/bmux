# bmux_windows_plugin

Bundled windows plugin for bmux.

## Overview

Implements window lifecycle management for bmux sessions. Windows are modeled as
server-side contexts and the plugin uses the host runtime API for context CRUD
operations. Tracks per-client active/previous window state so each connected
client can navigate windows independently.

## Commands

- `windows list` -- list windows in the current session
- `windows new [--name <name>]` -- create a new window
- `windows kill <target>` -- close a specific window
- `windows kill-all` -- close all windows in the session
- `windows switch <target>` -- switch the active window

## Services

- **`window-query/v1`** -- `list` windows
- **`window-command/v1`** -- `new` / `kill` / `kill_all` / `switch`
