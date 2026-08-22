# bmux tab-strip plugin

Bundled horizontal presentation of the authoritative ordered window list.

```toml
[plugins.settings."bmux.tab_strip"]
placement = "top" # or "bottom"
height = 1
order = 100
show_index = true
```

Disable it with `plugins.disabled = ["bmux.tab_strip"]`. The sidebar is an
independent plugin, so either presentation, both, or neither may be enabled.
