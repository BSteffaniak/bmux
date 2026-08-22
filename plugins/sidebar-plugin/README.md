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
heading = "Windows"
title_template = "{marker} {index}{name}"
description_template = ""
status_template = ""
```

Templates support `{marker}`, `{index}`, `{name}`, `{id}`, and `{active}`.
Descriptions wrap to two display-cell-safe lines; status renders on a separate
line when configured.

Disable it with `plugins.disabled = ["bmux.sidebar"]`. It composes through the
generic layout resolver with the independent tab-strip plugin.
