# bmux tab-strip plugin

Bundled full-width attach status/tab bar. It projects the authoritative ordered
window list together with neutral attach-local mode, role, follow, hint, and
optional session/context labels. It is enabled by default and reserves one row
at the bottom unless configured otherwise.

```toml
[plugins.settings."bmux.tab_strip"]
placement = "bottom" # or "top"
height = 1
order = 100
preset = "tab_rail" # "minimal" or "classic"
tab_label_max_width = 20
tab_template = "{name}"
show_session_name = false
show_context_name = false
show_mode = true
show_role = true
show_follow = true
show_hint = true
hover_highlight = true
hint_policy = "scroll_only" # "always" or "never"

[plugins.settings."bmux.tab_strip".layout]
density = "cozy" # or "compact"
left_padding = 1
right_padding = 1
tab_gap = 1
module_gap = 1
overflow_style = "arrows" # or "count"
align_active = "keep_visible" # or "focus_bias"

[plugins.settings."bmux.tab_strip".style]
separator_set = "angled_segments" # "plain" or "ascii"
prefer_unicode = true
force_ascii = false
dim_inactive = true
bold_active = true
underline_active = false
```

Optional `[plugins.settings."bmux.tab_strip".colors]` keys are `bar_bg`,
`bar_fg`, `tab_active_bg`, `tab_active_fg`, `tab_inactive_bg`,
`tab_inactive_fg`, `tab_hover_bg`, `tab_hover_fg`,
`tab_active_hover_bg`, `tab_active_hover_fg`, `module_bg`, `module_fg`,
`overflow_bg`, and `overflow_fg`. Values are `#RRGGBB` strings.

Templates support `{name}`, `{index}`, `{index0}`, `{session}`, `{marker}`,
`{id}`, and `{active}`. Double braces render literal braces. Unknown or
unterminated placeholders remain visible. Tabs pack to the terminal width,
retain the active item, and expose bounded interactive regions.

Interactions include click switching, hover feedback, drag reorder, wheel
navigation, middle-click inline rename, and a right-click Switch/Rename/Close
menu. Rename and menu keyboard input is captured by the plugin and mutations go
through generated `bmux.windows` clients.

Migration-era aliases remain accepted when the canonical key is absent:
`label_template`, `maximum_label_width`, `maximum_visible_tabs`, `show_index`,
and `show_compact_facts`. Canonical settings take precedence.

Disable the bar with `plugins.disabled = ["bmux.tab_strip"]`. Essential attach
notifications retain a neutral fallback when it is disabled. The sidebar is an
independent opt-in plugin.
