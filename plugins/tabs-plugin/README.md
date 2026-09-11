# bmux_tabs_plugin

Bundled tabs plugin for bmux.

## Overview

Implements tab lifecycle management for bmux sessions. Tabs are modeled as
server-side contexts and the plugin uses the host runtime API for context CRUD
operations. Tracks per-client active/previous tab state so each connected
client can navigate tabs independently.

## Commands

- `tabs list` -- list tabs in the current session
- `tabs new [--name <name>]` -- create a new tab
- `tabs rename [--name <name>]` -- rename the current tab; attach keybindings prompt when `--name` is omitted
- `tabs kill <target>` -- close a specific tab
- `tabs kill-all` -- close all tabs in the session
- `tabs switch <target>` -- switch the active tab

## Services

- **`tabs-state`** -- `list-tabs` (query)
- **`tabs-commands`** -- `new-tab` / `rename-tab` / `kill-tab` / `kill-all-tabs` / `switch-tab` (command)
- **`tabs-events`** -- pane-event stream
