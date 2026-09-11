# BMUX Concepts

This page gives a practical mental model for how bmux is structured so command
choices and troubleshooting steps are easier to reason about.

## Canonical terminology

This section is the vocabulary authority for BMUX code, contracts, configuration,
commands, UI labels, and documentation. Distinct layers keep distinct names even
when the ordinary single-terminal workflow maps them one-to-one.

- **Server**: long-lived control process that owns runtime state.
- **Runtime**: isolated BMUX execution environment selected with `--runtime`.
- **Workspace**: named grouping of tabs; switching workspaces does not itself
  allocate another runtime or PTY.
- **Tab**: user-facing attach unit backed by a context and, currently, one
  session. Its owning plugin is `bmux.tabs`; “window” is not a second resource.
- **Session**: runtime/process lifetime backing a tab.
- **Context**: attachable execution resource owned by the contexts plugin.
- **Pane**: terminal surface executing shell/program I/O.
- **Client**: one viewer/controller with its own view state.
- **Attachment**: a client's association with running work.
- **Connection**: transport connection, distinct from client and resource identity.
- **Tab bar**: optional presentation of tabs and status information, owned by
  `bmux.tab_bar`. A reusable status-bar control without tabs remains a status bar.

Native operating-system windows, Microsoft Windows, time windows, scrollback
windows, and third-party command names are unrelated terms and are not renamed.

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

BMUX core is domain-agnostic. Workspaces, tabs, and permissions are
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
