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
- `next-tab`, `prev-tab`, `goto-tab N`, `last-tab`, `close-current-tab` --
  walk the caller's personal tab view in the active workspace (an attachment's
  bound view; a CLI caller's default view where it already arranges the
  workspace). Callers without one (no principal, or acting for no client) walk
  the shared order. `close-current-tab` closes the tab for everyone, without
  asking (it has no default key binding; the tab bar's Close for everyone…
  asks first).
- `tab remove-from-view` (prefix `X`) -- remove the current tab from this
  attachment's view; it keeps running and stays discoverable. Every attachment
  bound to the same view that showed the tab moves to its neighbour (next,
  else previous). When the view of that workspace has no tab left, those
  attachments stay connected with no input target (nothing typed reaches any
  tab) until a tab is opened, created or restored.
- `tab undo-remove` / `undo-remove-from-view` (prefix `u`) -- put this
  attachment's last removed tab back in its place, and select it again when it
  was selected, if the tab still exists.
- `tab open <TAB_ID>` -- add a tab to this attachment's view if absent, then
  switch to it (the finder's choice)
- `tab add-to-view <TAB_ID>` -- add a tab to this attachment's view without
  switching
- `tab view list` / `list-views` -- your views, marking the default and this
  attachment's
- `tab view create <NAME> [--copy]` / `create-view` -- create an empty view
  (`--copy`: a copy of this attachment's view, with its order and labels);
  an attachment switches to it
- `tab view switch <VIEW>` / `switch-view` -- show another of your views in
  this attachment (keeping its tab when that view holds it, else moving to the
  view's first tab of the workspace)
- `tab view rename <NAME> [--view VIEW]`, `tab view delete [VIEW]`,
  `tab view set-default [VIEW]` -- manage views (default: this attachment's,
  or a CLI caller's default view). Deleting never touches tabs; attachments of
  the deleted view move to the default view. The default view cannot be
  deleted until another view is the default.
- `tab label <LABEL> [--tab TAB_ID]` / `set-tab-label`, `tab clear-label`
  / `clear-tab-label` -- rename a tab for this view only, or show its shared
  name again (`rename-tab` renames it for everyone)

Views are named by id or exact name; names are unique per user. Every
attachment starts on its user's default view; attachments sharing a view
share its arrangement and labels but each keeps its own selection.

While an attachment follows another client (`bmux.clients` `follow`), it shows
the leader's tab (and, for a global follow, each tab the leader switches to)
without adding it to its view, its projection says so, and removals or
deletions in its view do not move it. When the follow ends, the
tabs plugin selects the tab the attachment had before following again.

A tab is never terminated because no view references it: removing a reference
only changes that view. Closing a tab for everyone (`kill-tab`, the tab bar's
confirmed Close for everyone…) terminates it; every view then drops it and
each attachment that showed it moves to the neighbour in its own view.

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

## Tab states

The tabs plugin decides, on the server, the one state every view shows for
a tab, published in `bmux.tabs/tabs-status` (`state`, `state-since-ms`), most
pressing first:

- **needs input**: its agent needs an answer or approval, or a pane rang the
  bell or sent a notification since anyone looked at the tab;
- **failed**: a command exited non-zero, until someone looks at the tab (by
  default) or until the next command;
- **done**: its agent finished since anyone looked at the tab;
- **working**: its agent works, or a command runs and printed in the last
  three seconds;
- **stopped**: its agent finished or waits at its prompt, and that was seen;
- **idle**: otherwise.

An agent counts as finished once it has stayed idle for 1.5 seconds after
working, so a pause between steps or before a question is not a finish.
Looking at a tab means some attach client shows it while its terminal has
focus (`clients-presence-state-v1`); it marks everything the tab recorded as
seen, for every client. Recorded finishes, bells and failures, and how far
they were seen, are saved in the plugin's storage and survive restarts.

```toml
[plugins.settings."bmux.tabs"]
failed_clears = "when_seen" # or "on_next_command"
```

## Services

- **`tabs-state`** -- `list-tabs` (query)
- **`tabs-commands`** -- `new-tab` / `rename-tab` / `kill-tab` / `kill-all-tabs` / `switch-tab` (command)
- **`tabs-events`** -- pane-event stream
