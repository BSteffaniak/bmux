# Window presentation plugins

BMUX ships two independent window presentation plugins for normal `bmux attach`:

- `bmux.tab_strip` is enabled by default and owns the full horizontal
  status/tab row at the bottom.
- `bmux.sidebar` is bundled but opt-in and reserves a bounded vertical region
  at the left or right.

Both consume the authoritative ordered window state from `bmux.windows`;
presentation enablement does not change window lifecycle or ordering.

## Enablement combinations

The default is the full tab/status bar without the sidebar.

```toml
# Enable the optional sidebar as well
[plugins]
enabled = ["bmux.sidebar"]
```

```toml
# Sidebar only
[plugins]
enabled = ["bmux.sidebar"]
disabled = ["bmux.tab_strip"]
```

```toml
# Neither presentation; baseline attach remains available
[plugins]
disabled = ["bmux.tab_strip", "bmux.sidebar"]
```

## Full tab/status bar

```toml
[plugins.settings."bmux.tab_strip"]
placement = "bottom"            # "top" or "bottom"
height = 1                       # 1..=4 cells
order = 100                      # lower layout order allocates first
preset = "tab_rail"             # "minimal" or "classic"
tab_label_max_width = 20
tab_template = "{name}"
show_session_name = false
show_context_name = false
show_mode = true
show_role = true
show_follow = true
show_hint = true
hover_highlight = true
hint_policy = "scroll_only"     # "always" or "never"

[plugins.settings."bmux.tab_strip".layout]
density = "cozy"               # or "compact"
left_padding = 1
right_padding = 1
tab_gap = 1
module_gap = 1
overflow_style = "arrows"       # or "count"
align_active = "keep_visible"   # or "focus_bias"

[plugins.settings."bmux.tab_strip".style]
separator_set = "angled_segments" # "plain" or "ascii"
prefer_unicode = true
force_ascii = false
dim_inactive = true
bold_active = true
underline_active = false
```

The bar composes width-packed tabs with right-aligned mode, role, follow, and
conditional hint/message modules. Optional session/context modules occupy the
left side after tabs. Templates support `{name}`, `{index}`, `{index0}`,
`{session}`, `{marker}`, `{id}`, and `{active}` with Unicode-cell-safe width
limits and literal double braces.

Optional color keys under `[plugins.settings."bmux.tab_strip".colors]` cover
the bar, active/inactive/hover tabs, modules, and overflow using `#RRGGBB`
values. Migration-era aliases (`label_template`, `maximum_label_width`,
`maximum_visible_tabs`, `show_index`, `show_compact_facts`) remain accepted
when their canonical setting is absent.

Interactions include click switching, hover, drag reorder, wheel navigation,
middle-click inline rename, and a right-click Switch/Rename/Close menu. Domain
mutations use generated `bmux.windows` service clients.

## Sidebar

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
`{fact}`, `{fact_detail}`, and `{fact_icon}`. The generic layout resolver
composes it with the bar when enabled.

## Legacy configuration

Legacy `appearance.status_position` and `[status_bar]` are rejected with
migration diagnostics. Move those values into
`plugins.settings."bmux.tab_strip"` using the equivalent fields above.
