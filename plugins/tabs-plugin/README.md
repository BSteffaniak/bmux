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

## Spawn-directory inspection (in development)

Directory-aware creation is **not yet advertised by the bundled pane runtime**.
Nondefault policies can currently fail with an unavailable spawn-interface
error;
these settings are not a claim that the end-to-end feature is shipped. Missing
settings preserve the existing unspecified-CWD creation path.

Tabs parses independent `cwd.new_pane` and `cwd.new_tab` policies under
`[plugins.settings."bmux.tabs"]`. Each policy uses `source = "runtime-default"`,
`"home"`, `"path"` (with an absolute `path`), or `"source-pane"` (with a
required
`fallback = "home"` or `"runtime-default"`). Paths are execution-host paths, not
shell-expanded expressions. Fresh-shell options do not replace resurrection
metadata. Workspace policy is not implemented by this plugin.

Inspect without creating or selecting resources:

```sh
bmux explain-spawn-directory --operation pane
bmux explain-spawn-directory --operation tab --cwd /absolute/project
bmux explain-spawn-directory --operation pane --source-session UUID --source-pane UUID
```

The JSON report includes operation, structured policy, supplied source identity,
resolved `cwd` or `error`, and `creation_validated: false`. A null CWD with no
error
means unspecified runtime CWD, not necessarily home. Without an explicit source,
source-pane takes its configured fallback rather than another attachment's
focus.
Both source UUID options must be supplied together. `--cwd` overrides the
policy.
Resolution uses the plugin settings supplied by the host and the same resolver
as
creation; this command does not yet explain configuration-layer provenance,
observation method, or every fallback reason. A resolved fixed path has not been
validated for availability by startup. An error is included in the JSON report.

Do not edit a Home Manager `/nix/store` symlink to enable these settings. Edit
the
authoritative declarative source and activate it through its supported workflow
only once the required runtime capability is available; source edits alone do
not
verify effective behavior.

## Services

- **`tabs-state`** -- `list-tabs` (query)
- **`tabs-commands`** -- `new-tab` / `rename-tab` / `kill-tab` / `kill-all-tabs` / `switch-tab` (command)
- **`tabs-events`** -- pane-event stream
