# bmux sidebar plugin

Bundled vertical status board of the tabs: what each one is running, where,
on which git branch, and whether it needs you.

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
| `◆` yellow | blocked | the tab rang the bell or sent a notification since you last looked at it (an agent waiting for input, usually); its notification text replaces the directory line |
| `▲` red | failed | its last command exited non-zero (needs OSC 133 shell integration) |
| `◉` green | working | a command or agent is running and produced output in the last few seconds |
| `●` blue | done | an agent's output stopped since you last looked at the tab |
| `○` dim | idle | otherwise (the tab you are looking at shows a bright `●`) |

A tab counts as an agent when its running command's program is in `agents`.
"Since you last looked" is per attach client.

The data comes from the tabs plugin's `bmux.tabs/tabs-status` state, which it
derives on the server from each pane's terminal signals (title, OSC 7
directory, bell, OSC 9/777 notifications, OSC 133 exit status), its output, its
running command and `git status` of the focused pane's directory, so remote
attach clients see the server's repositories.

```toml
[plugins.settings."bmux.sidebar"]
placement = "left" # or "right"
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
agents = ["claude", "codex", "aider", "gemini", "opencode", "amp", "goose", "crush", "cursor-agent", "qwen", "copilot"]
visible = false            # closed when a client attaches
remember_visibility = false # start from the last state set on this machine
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
