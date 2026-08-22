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
maximum_label_width = 32         # Unicode display cells
```

The label template supports `{index}`, `{name}`, `{id}`, and `{active}`.
Double braces render literal braces. Clicking a visible item switches to that
window through the typed `bmux.windows` command service.

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
```

Sidebar templates support `{marker}`, `{index}`, `{name}`, `{id}`, and
`{active}`. Descriptions wrap to a bounded two-line region using Unicode display
width; status text receives its own row. Clicking an item switches windows.

The layout `order` values define deterministic composition when both plugins
are enabled. For example, the defaults allocate the tab strip before the
sidebar. Reversing the numeric values allocates the sidebar first.

## Current compatibility behavior

The legacy `[status_bar]` configuration continues to control the legacy attach
status modules during migration. Presentation plugin settings are deliberately
separate and do not reinterpret legacy keys. To avoid duplicate horizontal
window tabs while using `bmux.tab_strip`, set:

```toml
[status_bar]
enabled = false
```

This compatibility note remains necessary until the legacy status modules and
tab projection have fully migrated to plugin-owned surfaces.
