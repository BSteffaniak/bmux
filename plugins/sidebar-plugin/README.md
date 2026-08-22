# bmux sidebar plugin

Bundled vertical presentation of the authoritative ordered window list.

```toml
[plugins.settings."bmux.sidebar"]
placement = "left" # or "right"
width = 28
minimum_width = 16
maximum_width = 60
order = 200
show_index = true
```

Disable it with `plugins.disabled = ["bmux.sidebar"]`. It composes through the
generic layout resolver with the independent tab-strip plugin.
