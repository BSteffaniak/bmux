# bmux sidebar plugin

Bundled vertical status board of the tabs: what each one is running, where,
on which git branch, and whether it needs you.

Each tab is a card: a state glyph, the tab's name and, at the right, its
running command or state; below it, dimmed, the focused pane's directory and
git status (branch, `*` when dirty, `↑`/`↓` ahead and behind). The tab you
are
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
remember_visibility = false # start from the last state set on this machine
remember_width = true       # start from the width last resized to on this machine
remember_groups = true      # start with the groups last collapsed collapsed
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

With `grouping = "workspace"`, each workspace's tabs sit under a header that
sums up what they are doing: how many need input (`◆`), failed (`▲`), are
working (`◉`) and have stopped (`■`), most pressing first, or how many tabs it
holds when none is doing anything. Click a header (or press Enter or space on
it) to collapse or expand its group; left and right collapse and expand. A
collapsed header still shows its sum, and is marked when it hides the tab you
are looking at. Collapsed groups are remembered on this machine unless
`remember_groups = false`.

`scope = "current_workspace"` lists only the workspace of the tab you are
looking at, grouped or not, and follows you as you switch tabs.

Resize it by dragging the edge that faces the panes (the pointer shows a resize
cursor there, on terminals that support OSC 22 pointer shapes), or with
`plugin:bmux.sidebar:widen` and `plugin:bmux.sidebar:narrow` (`ctrl+a >` and
`ctrl+a <` by default, two cells per press; hold to repeat). The width stays
within `minimum_width` and `maximum_width`, and is remembered on this machine
unless `remember_width = false`.
