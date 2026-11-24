# bmux sidebar plugin

Bundled vertical status board of the tabs: what each one is running, where,
on which git branch, and whether it needs you.

Each tab is a card: a state glyph (a spinner while it works), the tab's name
and, at the right, its state: a solid pill when it needs you or failed,
otherwise the state or running command and how long it has been so. Below it,
dimmed, what it asks you, or its directory and git status (branch, `*` when
dirty, `↑`/`↓` ahead and behind); when space is short the directory shortens
to its last part, then gives way to git. The tab you are looking at has a bar
and a tinted background. The heading sums up what every listed tab is doing.

```text
 TABS                ◆1 ▲1 ⠹2 ■1
▌ bmux                ◆1 ▲1 ⠹1 ▾
▎ ⠹ sidebar         working 3m
▏   sidebar-plugin · main* ↑2
▏ ◆ review         [needs input]
▏   approve edit to lib.rs?
▏ ▲ tests            [exit 101]
▏   ~/bmux · main ↓3

▌ bcode                   ■1  ▾
▏ ■ triage          stopped 12m
▏   ~/bcode · feedback-triage
```

Every color comes from your theme: cards and header bands are blends of its
background toward its foreground or accent, so they read the same on light
and dark themes, and states and workspaces use your terminal's own colors.
Below 20 columns cards are one line and their state is their glyph.

States, most urgent first:

| Glyph      | State   | When                                                                                                                                                                                         |
| ---------- | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `◆` yellow | blocked | its agent needs you to answer or approve something, or (any other program) the tab rang the bell or sent a notification since you last looked at it; the message replaces the directory line |
| `▲` red    | failed  | its last command exited non-zero (needs OSC 133 shell integration)                                                                                                                           |
| `◉` green  | working | its agent is working, or another command is running and produced output in the last few seconds                                                                                              |
| `■` blue   | stopped | its agent is not working: it finished, or waits at its prompt                                                                                                                                |
| `○` dim    | idle    | otherwise                                                                                                                                                                                    |

An agent's state shows what the agent is doing whichever tab you are looking
at; the selected tab is marked by its tinted card and accent bar, never by its
state glyph.

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

```toml
[plugins.settings."bmux.sidebar"]
placement = "left" # or "right"
grouping = "none"          # or "workspace": tabs under a header per workspace
scope = "all_workspaces"   # or "current_workspace": only the shown tab's workspace
width = 28         # until you resize it
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
visible = false            # closed when a client attaches
remember_visibility = true  # start from the last state set on this machine
remember_width = true       # start from the width last resized to on this machine
remember_groups = true      # start with the groups last collapsed collapsed
working_indicator = "spinner" # or "static": a still ◉
density = "comfortable"     # or "compact": one line per tab

[plugins.settings."bmux.sidebar".workspace_colors]
bmux = "magenta"            # a terminal color name, or "#rrggbb"
```

Templates support `{state_icon}`, `{state}`, `{name}`, `{command}`, `{cwd}`,
`{branch}`, `{detail}`, `{marker}`, `{index}`, `{id}`, `{active}`, `{fact}`,
`{fact_detail}` and `{fact_icon}`.
Descriptions wrap to two display-cell-safe lines; status renders on a separate
line when configured.

It is enabled by default but starts hidden; open it with `ctrl+alt+s`, or set
`visible = true` to show it on attach. Disable it with
`plugins.disabled = ["bmux.sidebar"]`. It composes through the generic layout
resolver with the independent tab-bar plugin.

Show or hide it in one attach client with `plugin:bmux.sidebar:toggle`
(`ctrl+alt+s` by default), `plugin:bmux.sidebar:show` or
`plugin:bmux.sidebar:hide`. See `docs/tab-presentations.md` for details.

Click a tab, or press Enter on it, to show it, in whichever workspace it is
(through the workspaces plugin's `activate-tab`).

With `grouping = "workspace"`, each workspace's tabs sit under a header, a
band across the sidebar marked with the workspace's color (one picked from
its name unless `workspace_colors` sets it), which also runs down its tabs as
a thin rail; a blank row sets each group off from the next. While a group's
tabs scroll under the top, its header stays pinned there. The header sums up
what its tabs are doing: how many need input (`◆`), failed (`▲`), are
working (`◉`) and have stopped (`■`), most pressing first, or how many tabs it
holds when none is doing anything. Click a header (or press Enter or space on
it) to collapse or expand its group; left and right collapse and expand. A
collapsed header still shows its sum, and is marked when it hides the tab you
are looking at. Collapsed groups are remembered on this machine unless
`remember_groups = false`.

`scope = "current_workspace"` lists only the workspace of the tab you are
looking at, grouped or not, and follows you as you switch tabs.

Clicking a tab (or pressing Enter on it) shows it and leaves the keyboard with
that tab's pane. The sidebar has the keyboard only while you work in it: with
`behavior.mouse.focus_on_hover`, when the pointer enters it, and otherwise when
you click a header; its heading lights up and the row the keyboard is on is
highlighted, and the panes are drawn unfocused. Up, Down, Home and End move
through the rows; Escape, a key the sidebar does not use (typing), or moving the
pointer onto a pane hands the keyboard back to the pane.

One column faces the panes. Drag it sideways to resize the sidebar. When the
tabs do not fit, it also carries the scrollbar, colored from your theme (its
track in the inactive border color, its thumb in the cursor color). It stays a
slim bar against the panes until the sidebar has the keyboard or the pointer is
on that column, where it widens: drag the thumb (a grab hand) to scroll, or
click the track to jump there.

Resize it by dragging the edge that faces the panes (the pointer shows a resize
cursor there, on terminals that support OSC 22 pointer shapes), or with
`plugin:bmux.sidebar:widen` and `plugin:bmux.sidebar:narrow` (`ctrl+a >` and
`ctrl+a <` by default, two cells per press; hold to repeat). The width stays
within `minimum_width` and `maximum_width`, and is remembered on this machine
unless `remember_width = false`.
