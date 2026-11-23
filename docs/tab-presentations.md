# Tab presentation plugins

BMUX ships two independent tab presentation plugins for normal `bmux attach`:

- `bmux.tab_bar` is enabled by default and owns the full horizontal
  status/tab row at the bottom.
- `bmux.sidebar` is also enabled by default but starts hidden; when shown it
  reserves a bounded vertical region at the left or right.

Both consume the authoritative ordered tab state from `bmux.tabs`;
presentation enablement does not change tab lifecycle or ordering.

## Enablement combinations

By default both are loaded: the tab/status bar is shown and the sidebar is
hidden until you open it (`ctrl+alt+s`).

```toml
# Show the sidebar on attach as well
[plugins.settings."bmux.sidebar"]
visible = true
```

```toml
# Sidebar only
[plugins]
disabled = ["bmux.tab_bar"]

[plugins.settings."bmux.sidebar"]
visible = true
```

```toml
# Tab bar only, no sidebar at all
[plugins]
disabled = ["bmux.sidebar"]
```

```toml
# Neither presentation; baseline attach remains available
[plugins]
disabled = ["bmux.tab_bar", "bmux.sidebar"]
```

## Showing and hiding at runtime

Enabling a presentation loads it; whether it is shown can change while you are
attached. Each plugin has `toggle`, `show` and `hide` commands:

| Command                      | Default key  |
| ---------------------------- | ------------ |
| `plugin:bmux.tab_bar:toggle` | `ctrl+alt+b` |
| `plugin:bmux.sidebar:toggle` | `ctrl+alt+s` |
| `plugin:bmux.tab_bar:show`   | (unbound)    |
| `plugin:bmux.tab_bar:hide`   | (unbound)    |
| `plugin:bmux.sidebar:show`   | (unbound)    |
| `plugin:bmux.sidebar:hide`   | (unbound)    |

The sidebar can also be resized: drag the edge facing the panes, or use
`plugin:bmux.sidebar:widen` (`ctrl+a >`) and `plugin:bmux.sidebar:narrow`
(`ctrl+a <`). Its width is remembered on this machine unless
`remember_width = false`.

They are also listed in the command palette. Visibility belongs to one attach
client: hiding the sidebar in one terminal leaves other clients of the same
session as they were. A hidden presentation gives its rows or columns back to
the panes. Tab-bar toasts and attach status messages still appear while the bar
is hidden.

Both plugins take the same two settings:

```toml
[plugins.settings."bmux.sidebar"]   # or "bmux.tab_bar"
visible = true                     # shown on attach (sidebar default: false)
remember_visibility = false        # start from the last state set on this machine
```

With `remember_visibility = true`, every toggle is saved under the bmux state
directory (`plugins/<plugin id>/visibility`), and the next attach on that
machine starts from it instead of `visible`. Clients on other machines keep
their own.

## Full tab/status bar

```toml
[plugins.settings."bmux.tab_bar"]
placement = "bottom"            # "top" or "bottom"
# height = 1                    # legacy fixed-height single-row mode
order = 100                      # lower layout order allocates first
preset = "tab_rail"             # "minimal" or "classic"
tab_label_max_width = "unlimited" # default; or 1..=65535 terminal cells
tab_template = "{name}"
show_session_name = false
show_context_name = false
show_mode = true
show_role = true
show_follow = true
show_hint = true
hover_highlight = true
hint_policy = "scroll_only"     # "always" or "never"

[plugins.settings."bmux.tab_bar".layout]
wrap = true
min_rows = 1
max_rows = 3
density = "cozy"               # or "compact"
left_padding = 1
right_padding = 1
tab_gap = 1
module_gap = 1
overflow_style = "arrows"       # or "count"
align_active = "keep_visible"   # or "focus_bias"

[plugins.settings."bmux.tab_bar".style]
separator_set = "angled_segments" # "plain" or "ascii"
prefer_unicode = true
force_ascii = false
dim_inactive = true
bold_active = true
underline_active = false

# Status items contributed by other plugins (bmux.tab_bar status-items service)
[plugins.settings."bmux.tab_bar".status_items]
show_external = true
max_items = 6                   # 0..=32; highest priority first
max_item_width = 32             # 1..=128 terminal cells per item

# Toasts raised by plugins (bmux.tab_bar notifications service)
[plugins.settings."bmux.tab_bar".notifications]
enabled = true
max_visible = 3                 # 1..=8
width = 44                      # 16..=120 terminal cells
```

Tabs wrap between labels in authoritative order, left-to-right then
top-to-bottom,
for either placement. The bar grows and shrinks from `layout.min_rows` to
`layout.max_rows` (defaults 1 and 3; valid range `1 <= min_rows <= max_rows <=
16`).
Workspace and status modules appear only on the first row. Oversized labels clip
to one row; overflow navigation remains available at the ceiling and selection
reveals the active tab. Terminal allocation can reduce the visible row count.
Set `layout.wrap = false` for one row. An explicit legacy `height` retains its
fixed-height, single-text-row behavior; combining it with `wrap`, `min_rows`, or
`max_rows` is rejected. Remove `height` when migrating to adaptive rows.

The bar composes width-packed tabs with right-aligned mode, role, follow, and
conditional hint/message modules. Optional session/context modules occupy the
left side after tabs. Templates support `{name}`, `{index}`, `{index0}`,
`{session}`, `{marker}`, `{id}`, and `{active}` with Unicode-cell-safe width
limits and literal double braces. Tab names have no explicit width cap by
default
(or with `tab_label_max_width = "unlimited"`); the strip still clips to
available
terminal space. Set a positive integer such as `300` to cap names in terminal
cells.

Optional color keys under `[plugins.settings."bmux.tab_bar".colors]` cover
the bar, active/inactive/hover tabs, modules, and overflow using `#RRGGBB`
values. Use `tab_template`, `tab_label_max_width`, `max_tabs`, `show_tab_index`,
and `show_facts`; former aliases are rejected with an explicit error.

Interactions include click switching, hover, drag reorder, wheel navigation,
middle-click inline rename, and a right-click Switch/Rename/Close menu. Domain
mutations use generated `bmux.tabs` service clients.

Other plugins, including dynamically loaded native plugins, can add right-side
status modules and raise toasts through the typed `bmux.tab_bar` services in
`bmux_tab_bar_plugin_api`. Contributed items render to the left of the
built-in modules in priority order, use the module colors (tinted by severity),
take at most half the bar, and drop lowest-priority-first when space runs out.
See `plugins/tab-bar-plugin/README.md#plugin-contributions` for the API.

## Sidebar

```toml
[plugins.settings."bmux.sidebar"]
placement = "left"              # "left" or "right"
width = 28
minimum_width = 16
maximum_width = 60
order = 200
show_index = true
heading = "Tabs"
title_template = "{state_icon} {name}"
description_template = "{detail}"
status_template = ""
maximum_visible_items = 20
content_height = false
collapse_below_width = 80
collapsed_width = 8
visible = false
remember_visibility = false
```

Each tab is a card: a state glyph, the tab's name and, at the right, its
running command or state; below it, dimmed, the focused pane's directory and
git status (branch, `*` when dirty, `↑`/`↓` ahead and behind). The tab you are
looking at has an accent bar and a tinted background.

```text
 TABS                         4
▎ ● editor                 nvim
▎   ~/bmux · main* ↑2
  ◉ claude              working
    ~/bmux/plugins
  ▲ build              exit 101
    ~/build
  ◆ review          needs input
    approve edit?
```

States, most urgent first:

| Glyph | State | When |
| --- | --- | --- |
| `◆` yellow | blocked | its agent needs you to answer or approve something, or (any other program) the tab rang the bell or sent a notification since you last looked at it; the message replaces the directory line |
| `▲` red | failed | its last command exited non-zero (needs OSC 133 shell integration) |
| `◉` green | working | its agent is working, or another command is running and produced output in the last few seconds |
| `●` blue | done | its agent finished since you last looked at the tab |
| `○` dim | idle | otherwise (the tab you are looking at shows a bright `●`) |

A tab's agent state comes from the agents plugin (`bmux.agents`, see
`plugins/agents-plugin/README.md`), which recognizes coding agents in each
pane's foreground and reads what they are doing from their screens and their
own reports; a tab with several shows its most pressing. "Since you last
looked" is per attach client.

The data comes from the tabs plugin's `bmux.tabs/tabs-status` state, which it
derives on the server from each pane's terminal signals (title, OSC 7
directory, bell, OSC 9/777 notifications, OSC 133 exit status), its output,
the running command and directory bmux's shell integration reports (bash, zsh,
fish and nu, on by default through `behavior.pane_shell_integration`) and
`git status` of the focused pane's directory, so remote attach clients see the
server's repositories. Without shell integration a tab shows no running command
(agents are still recognized, from the pane's foreground processes).

A tab's directory is the one its shell reports. A program's OSC 7 directory is
shown instead while that program runs (an editor, or a remote shell, shown as
`host:path`) and dropped when it exits. Since any output can carry OSC 7, git
only ever runs in the directory the shell itself reports, with
`core.fsmonitor` disabled.

Sidebar templates support `{state_icon}`, `{state}`, `{name}`, `{command}`,
`{cwd}`, `{branch}`, `{detail}`, `{marker}`, `{index}`, `{id}`, `{active}`,
`{fact}`, `{fact_detail}`, and `{fact_icon}`. The generic layout resolver
composes it with the bar when enabled.

## Legacy configuration

Legacy `appearance.status_position` and `[status_bar]` are rejected with
migration diagnostics. Move those values into
`plugins.settings."bmux.tab_bar"` using the equivalent fields above.
