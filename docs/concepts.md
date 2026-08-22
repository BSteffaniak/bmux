# BMUX Concepts

This page gives a practical mental model for how bmux is structured so command
choices and troubleshooting steps are easier to reason about.

## Core Objects

- **Server**: long-lived control process that owns runtime state.
- **Workspace**: named metadata grouping over tabs; switching workspaces changes
  the visible tab set without allocating another runtime or PTY.
- **Tab**: user-facing attach unit backed by a context and, currently, one
  session. Internal plugin interfaces retain the historical `windows` name.
- **Session**: runtime/process lifetime backing a tab.
- **Context**: generic attachable execution resource used by plugins.
- **Pane**: terminal surface executing shell/program I/O.
- **Client**: one attached viewer/controller with its own view state.

## Workspace and Finder Settings

Workspace deletion and tab-finder behavior are configured through plugin
settings:

```toml
[plugins.settings."bmux.workspaces"]
# "delete" (default) removes an empty workspace; "keep_empty" retains it.
on_last_tab_closed = "delete"

[plugins.settings."bmux.finder"]
# Search every workspace by default, or use "current_workspace".
scope = "all_workspaces"
include_workspace_name = true
# "fuzzy" (default), "prefix", or "substring".
match_mode = "fuzzy"
entry_format = "{workspace}/{tab}"
```

`entry_format` supports the `{workspace}` and `{tab}` placeholders. Finder
matching uses the workspace name only when `include_workspace_name` is true.

## Architecture Boundary

BMUX core is domain-agnostic. Workspaces, tabs/windows, and permissions are
plugin domains. Core runtime behavior should stay generic, and plugins should
carry domain logic through plugin/service interfaces.

## Command Surfaces

- **Task-first commands**: `bmux connect`, `bmux setup`, `bmux host`
- **Grouped commands**: `bmux session ...`, `bmux server ...`, `bmux remote ...`
- **Automation commands**: `bmux playbook ...`

## Quick Validation Examples

```bmux-cli
bmux setup --check
bmux server status --json
bmux list-sessions --json
```
