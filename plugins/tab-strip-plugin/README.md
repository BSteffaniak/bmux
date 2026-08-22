# bmux tab-strip plugin

Bundled horizontal presentation of the authoritative ordered window list.

```toml
[plugins.settings."bmux.tab_strip"]
placement = "top" # or "bottom"
height = 1
order = 100
show_index = true
label_template = "{index}{name}"
maximum_label_width = 32
```

Labels support `{index}`, `{name}`, `{id}`, and `{active}` with Unicode
cell-safe width limits.

Disable it with `plugins.disabled = ["bmux.tab_strip"]`. The sidebar is an
independent plugin, so either presentation, both, or neither may be enabled.
