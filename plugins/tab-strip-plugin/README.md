# bmux tab-strip plugin

Bundled horizontal presentation of the authoritative ordered window list.

```toml
[plugins.settings."bmux.tab_strip"]
placement = "bottom" # or "top"
height = 1
order = 100
show_index = true
label_template = "{index}{name}"
maximum_label_width = 32
maximum_visible_tabs = 8
show_compact_facts = false
```

Middle-clicking a tab starts inline rename; type the replacement, press Enter to
commit through the typed rename command, or Escape to cancel. Labels support `{index}`, `{name}`, `{id}`, and `{active}` with Unicode
cell-safe width limits.

Disable it with `plugins.disabled = ["bmux.tab_strip"]`. The sidebar is an
independent plugin, so either presentation, both, or neither may be enabled.
