# BMUX Concepts

This page gives a practical mental model for how bmux is structured so command
choices and troubleshooting steps are easier to reason about.

## Canonical terminology

This section is the vocabulary authority for BMUX code, contracts,
configuration,
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
[plugins.settings."bmux.tabs"]
# When `close-current-tab` closes the last tab of a workspace (for everyone,
# not merely the last tab of your personal view): "delete" (default) also
# removes the workspace; "keep_empty" retains it.
on_last_tab_closed = "delete"

[plugins.settings."bmux.finder"]
# Search every workspace by default, or use "current_workspace".
scope = "all_workspaces"
include_workspace_name = true
# "fuzzy" (default), "prefix", or "substring".
match_mode = "fuzzy"
entry_format = "{workspace}/{tab}"
# Opening order: "last_visited" (default), "workspace_tab", "alphabetical".
sort_order = "last_visited"
# "relevance" (default), "inherit", or an explicit opening-order value.
filtered_sort_order = "relevance"
# "hidden" (default), "last", or "in_order".
current_tab = "hidden"
# Wrap Up/Down (and Ctrl-P/Ctrl-N) through filtered results; false clamps at ends.
wrap_selection = true
```

`entry_format` supports the `{workspace}` and `{tab}` placeholders. Finder
matching uses the workspace name only when `include_workspace_name` is true.

Finder defaults now hide the current tab and show other tabs across all
workspaces
in personal most-recently-visited order when the query is empty or whitespace.
Filtering defaults to `relevance`: exact names/paths, prefixes, word-boundary
matches, substrings, then scattered fuzzy matches. Equal-quality contiguous
matches use opening order to break ties; loose matches use fuzzy score first.
Clearing the query restores opening order. Set `filtered_sort_order = "inherit"`
to preserve strict recency while filtering. `workspace_tab` follows workspace
and tab
arrangements; `alphabetical` sorts displayed labels. These projections never
rearrange the tab bar. `current_tab = "last"` places the current tab after other
matches regardless of ranking; `in_order` treats it like any other result.

The finder is discovery over every shared tab, not only those in your
personal tab view. Each entry's detail says whether it is in your view, and
tabs in your view are listed before the others. Choosing a tab adds it to your
view when it is absent, then switches to it. `bmux finder add-to-view` (or
`plugin:bmux.finder:add-to-view`) lists only the tabs your view lacks and adds
the chosen one without switching. Without a personal view (no attachment or
principal) the finder switches as it always did.

Removing a tab from your view (`remove-from-view`, prefix `X`, or the tab
bar's Remove from view) never closes it; `undo-remove-from-view` (prefix `u`)
puts your last removal back. Attachments sharing that view move off the
removed tab to its neighbour. When your view of a workspace has no tab left,
the attachment stays connected with no input target: nothing you type reaches
a tab until you open one from the finder, create one or undo. Closing a tab
for everyone terminates it and removes it from every view.

### Personal tab views

Each user (principal) arranges the shared tabs in one or more personal tab
views. A view decides which tabs you see and in what order; it never decides
whether a tab runs.

- Your default view is the one every new attachment shows; it is created the
  first time you attach. `bmux tab view set-default VIEW` makes another view
  the default. The default view cannot be deleted: make another view the
  default first.
- Attachments of one principal bound to the same view share its arrangement:
  adding, removing and reordering tabs and setting labels show in all of them.
  They do not share selection: each keeps its own current tab and history.
- `bmux tab view create NAME` creates an empty view (every workspace that has
  tabs starts empty), or with `--copy` a copy of your current view's tabs,
  order and labels; from an attachment it also switches that attachment to
  it. `bmux tab view switch VIEW`, `rename`, `delete` and `list` manage views
  by name or id. The view picker (`plugin:bmux.finder:views`, prefix `V`)
  switches views or creates one. The tab bar names the view when it is not
  the default.
- Switching or deleting a view never touches tabs. An attachment keeps its tab
  when the view it moves to holds it, otherwise moves to that view's first tab
  in the workspace, or has no input target when there is none. Attachments of
  a deleted view move to the default view.
- A personal label (`bmux tab label LABEL`, or Rename for me… in the tab bar)
  renames a tab in one view only; Rename for everyone… (`rename-tab`) renames
  the tab itself. `bmux tab clear-label` (Clear my label) shows the shared
  name again. The finder shows the label and searches both names.
- Removing a tab from your view (`bmux tab remove-from-view`, prefix `X`, or
  Remove from view in the tab bar) leaves it running and findable; undo
  (`bmux tab undo-remove`, prefix `u`) puts it back in place. Closing a tab for
  everyone (Close for everyone…, which asks first, or `kill-tab`) ends it and
  drops it from every view. No tab ever ends because no view holds it.
- When the last tab of a workspace leaves your view, the attachment stays
  connected with no input target: the tab bar says "View empty", and typing
  reaches no tab until you open or add one from the finder or undo.
- The finder lists every tab and marks whether your view holds it. Choosing a
  tab adds it to your view when absent, then switches to it;
  `plugin:bmux.finder:add-to-view` adds a tab without switching.
- Views, their order, labels and default are stored by the server and survive
  reconnecting and restarting it. Every attach starts on your default view;
  removed tabs stay removed and closed tabs are gone.
- Commands run without an attachment (the CLI) act on your default view.
- Following another client (`plugin:bmux.clients:follow CLIENT --global` from
  an attachment, or `bmux attach --follow`) shows the leader's tab, and each
  tab the leader switches to, without adding it to your view; the tab bar says
  so ("Following … (not in your view)"). While you follow, removals and
  deletions in your view do not move you. Ending the follow
  (`plugin:bmux.clients:unfollow`, or the leader disconnecting) returns you to
  the tab you had when the follow began.

Visit history belongs to the invoking client, not other attachments. It is
transient, cleared on disconnect, and not restored across server restarts.
Unvisited tabs follow visited tabs in arrangement order. Opening or cancelling
the finder does not record visits, and its ordering remains fixed while open.
Older providers lacking the versioned catalog/history interfaces return explicit
errors rather than silently substituting another order. To approximate the old
finder ordering, use `alphabetical`, `relevance`, and `in_order` respectively.

### When the server is slow or goes away

An attachment never ends because a request failed. A banner on the top row
says what is wrong until it is resolved:

- "server not responding — retrying…": requests keep timing out; input still
  goes to your panes. Over a paired remote host (`--target`), a network path
  can die without a reset, so three timeouts in a row (or timeouts for 15
  seconds) with no answer in between count as a lost connection instead.
- "connection lost — reconnecting (attempt N, next in Xs)": the connection
  dropped. The last screen stays up and the client reconnects in place, with
  backoff, to the same session and tab, then redraws it. A bare `bmux` starts
  the server again if none runs (unless it was stopped on purpose); an explicit
  `bmux attach` waits for one. `q` quits, `d` detaches locally; other keys are
  dropped (there is nothing to send them to). A paired remote host
  (`--target`) is tried for at least two minutes, and not at all once it has
  revoked this device; the client then exits with the command that resumes
  the session.
- "view out of sync with the server: … press r to retry": the server refused
  to resync the view several times in a row; it is not retried again until
  you press `r`.

If the session no longer exists once the server is back, the client exits
with "session no longer exists — attach ended".

## Architecture Boundary

BMUX core is domain-agnostic. Workspaces, tabs, and permissions are
plugin domains. Core runtime behavior should stay generic, and plugins should
carry domain logic through plugin/service interfaces.

## Command Surfaces

- **Task-first commands**: `bmux connect`, `bmux attach`, `bmux new-session`
- **Grouped commands**: `bmux session ...`, `bmux server ...`, `bmux remote ...`
- **Automation commands**: `bmux playbook ...`

## Quick Validation Examples

```bmux-cli
bmux doctor
bmux server status --json
bmux list-sessions --json
```
