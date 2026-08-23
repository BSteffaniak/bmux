# Window presentation plugins

BMUX ships two independent window presentation plugins for normal `bmux attach`:

- `bmux.tab_strip` reserves a horizontal row at the top or bottom.
- `bmux.sidebar` reserves a bounded vertical region at the left or right.

Both are bundled and enabled by default. They consume the authoritative ordered
window state from `bmux.windows`; disabling a presentation does not change
window lifecycle or ordering.

## Enablement combinations

Disable either plugin with the normal plugin configuration:

```toml
# Tab strip only
[plugins]
disabled = ["bmux.sidebar"]
```

```toml
# Sidebar only
[plugins]
disabled = ["bmux.tab_strip"]
```

```toml
# Neither presentation (baseline terminal attach remains available)
[plugins]
disabled = ["bmux.tab_strip", "bmux.sidebar"]
```

With neither ID disabled, both presentations are enabled. Disable
`bmux.windows` separately only when intentionally using baseline attach without
the authoritative window facade.

## Tab strip settings

```toml
[plugins.settings."bmux.tab_strip"]
placement = "top"               # "top" or "bottom"
height = 1                       # 1..=4 cells
order = 100                      # lower layout order is allocated first
show_index = true
label_template = "{index}{name}"
maximum_label_width = 32
maximum_visible_tabs = 8
show_compact_facts = false
```

The label template supports `{index}`, `{name}`, `{id}`, `{active}`, and
`{fact}`. When `show_compact_facts` is enabled, the highest-priority retained
window fact supplies `{fact}` and its semantic role styles the compact tab.
Double braces render literal braces. `maximum_visible_tabs` bounds retained tab
projection; overflow markers show hidden leading/trailing items, wheel input
scrolls the visible window, and authoritative active-window changes realign it.
Clicking a visible item switches to that window through the typed
`bmux.windows` command service.

## Sidebar settings

```toml
[plugins.settings."bmux.sidebar"]
placement = "left"              # "left" or "right"
width = 28
minimum_width = 16
maximum_width = 60
order = 200
show_index = true
heading = "Windows"
title_template = "{marker} {index}{name}"
description_template = ""
status_template = ""
maximum_visible_items = 20
content_height = false
collapse_below_width = 80
collapsed_width = 8
```

Sidebar templates support `{marker}`, `{index}`, `{name}`, `{id}`, `{active}`,
`{fact}`, `{fact_detail}`, and `{fact_icon}`. The highest priority retained fact
for entity `("bmux.windows", window UUID)` supplies text and semantic role;
roles map to neutral/idle/active/success/warning/attention/error terminal styles.
Descriptions wrap to a bounded two-line region using Unicode display width;
status text receives its own row. `maximum_visible_items` bounds retained
card projection; wheel input scrolls the virtual window and authoritative active
window changes realign it automatically. `content_height = false` paints the
full resolved allocation; enabling it clips background/border paint to the
bounded visible-card height while retaining the same layout reservation. Below `collapse_below_width` terminal
columns, generic layout reserves `collapsed_width` instead of the preferred
width. Clicking an item switches windows.

The layout `order` values define deterministic composition when both plugins
are enabled. For example, the defaults allocate the tab strip before the
sidebar. Reversing the numeric values allocates the sidebar first.

## Removed legacy status placement

Legacy `appearance.status_position` no longer controls attach geometry and is
rejected with a migration diagnostic. Configure the tab strip directly instead:

```toml
[plugins.settings."bmux.tab_strip"]
placement = "top" # or "bottom"
```

Legacy `[status_bar]` configuration has been removed and is rejected with a
migration diagnostic. Configure normal attach presentation through the
`bmux.tab_strip` and `bmux.sidebar` plugin settings documented above.
