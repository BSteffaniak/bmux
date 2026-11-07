# bmux tab-bar plugin

Bundled full-width attach status/tab bar. It projects the authoritative ordered
tab list together with neutral attach-local mode, role, follow, hint, and
optional session/context labels. It is enabled by default and reserves one row
at the bottom unless configured otherwise.

```toml
[plugins.settings."bmux.tab_bar"]
placement = "bottom" # or "top"
# height = 1 # legacy fixed-height single-row mode; do not combine with row settings
order = 100
preset = "tab_rail" # "minimal" or "classic"
tab_label_max_width = "unlimited" # default; or 1..=65535 terminal cells
tab_template = "{name}"
show_session_name = false
show_context_name = false
show_mode = true
show_role = true
show_follow = true
show_hint = true
hover_highlight = true
hint_policy = "scroll_only" # "always" or "never"

[plugins.settings."bmux.tab_bar".layout]
wrap = true
min_rows = 1
max_rows = 3
density = "cozy" # or "compact"
left_padding = 1
right_padding = 1
tab_gap = 1
module_gap = 1
overflow_style = "arrows" # or "count"
align_active = "keep_visible" # or "focus_bias"

[plugins.settings."bmux.tab_bar".style]
separator_set = "angled_segments" # "plain" or "ascii"
prefer_unicode = true
force_ascii = false
dim_inactive = true
bold_active = true
underline_active = false

[plugins.settings."bmux.tab_bar".status_items]
show_external = true # render items contributed by other plugins
max_items = 6        # 0..=32
max_item_width = 32  # 1..=128 cells per item

[plugins.settings."bmux.tab_bar".notifications]
enabled = true  # render plugin toasts
max_visible = 3 # 1..=8
width = 44      # 16..=120 cells
```

Optional `[plugins.settings."bmux.tab_bar".colors]` keys are `bar_bg`,
`bar_fg`, `tab_active_bg`, `tab_active_fg`, `tab_inactive_bg`,
`tab_inactive_fg`, `tab_hover_bg`, `tab_hover_fg`,
`tab_active_hover_bg`, `tab_active_hover_fg`, `module_bg`, `module_fg`,
`overflow_bg`, and `overflow_fg`. Values are `#RRGGBB` strings.

Templates support `{name}`, `{index}`, `{index0}`, `{session}`, `{marker}`,
`{id}`, and `{active}`. Double braces render literal braces. Unknown or
unterminated placeholders remain visible. Tabs pack to the terminal width,
retain the active item, and expose bounded interactive regions.

Tabs wrap into 1–3 rows by default, growing only as needed. Configure
`layout.min_rows` and `layout.max_rows` within 1–16, or set `layout.wrap =
false`
for one row. Labels do not split across rows; oversized labels clip, and
overflow
navigation remains available at the row ceiling. Status modules appear once.
Explicit legacy `height` preserves fixed-height single-row behavior and cannot
be combined with the new row settings. See `docs/tab-presentations.md`.

Interactions include click switching, hover feedback, drag reorder, wheel
navigation, middle-click inline rename, and a right-click Switch/Rename/Close
menu. Rename and menu keyboard input is captured by the plugin and mutations go
through generated `bmux.tabs` clients.

Migration-era aliases remain accepted when the canonical key is absent:
`label_template`, `maximum_label_width`, `maximum_visible_tabs`, `show_index`,
and `show_compact_facts`. Canonical settings take precedence.

Disable the bar with `plugins.disabled = ["bmux.tab_bar"]`. Essential attach
notifications retain a neutral fallback when it is disabled. The sidebar is an
independent opt-in plugin.

## Plugin contributions

Any plugin — bundled, process, or a dynamically loaded native (`cdylib`)
plugin — can contribute status modules to the bar and raise attach toasts.
Rust state cannot cross the dylib boundary, so these are typed BPDL services
generated in the `bmux_tab_bar_plugin_api` crate (schema:
`plugins/tab-bar-plugin-api/bpdl/tab-bar-plugin.bpdl`). Depend on that crate,
not on this plugin.

| Interface (`interface_id`) | Kind    | Capability                  | Operations                                                   |
| -------------------------- | ------- | --------------------------- | ------------------------------------------------------------ |
| `status-items`             | command | `bmux.tab_bar.status.write` | `set-status-item`, `clear-status-item`, `clear-status-items` |
| `status-items-state`       | query   | `bmux.tab_bar.status.read`  | `list-status-items`                                          |
| `notifications`            | command | `bmux.tab_bar.notify`       | `notify`, `dismiss`                                          |

Declare the capabilities you use in your manifest:

```toml
required_capabilities = ["bmux.tab_bar.status.write", "bmux.tab_bar.notify"]
```

Call the generated clients from any `ServiceCaller` (command, service, or
lifecycle context):

```rust
use bmux_plugin::{ServiceCallerDispatchClient, block_on_typed_dispatch};
use bmux_tab_bar_plugin_api::{notifications, status_items, status_items_list::Severity};

let mut client = ServiceCallerDispatchClient::new(&context);
// Outer Result: transport/permission failure. Inner Result: validation.
block_on_typed_dispatch(status_items::client::set_status_item(
    &mut client,
    "build".into(),         // id, unique per calling plugin
    "build ok".into(),      // text
    Severity::Success,      // normal | info | success | warning | critical
    10,                     // priority: higher renders first, drops last
    None,                   // optional tooltip (listed, not yet rendered)
))??;
block_on_typed_dispatch(notifications::client::notify(
    &mut client,
    "Deploy finished".into(),
    Some("all checks green".into()),
    Severity::Info,
    Some(4_000),            // ttl in ms; default 5000, clamped to 500..=60000
))??;
```

Semantics:

- Items are namespaced by the caller plugin id the host authenticated
  (`ServiceRequest::caller_plugin_id`), so plugins cannot replace, clear, or
  dismiss each other's entries. The host derives the id from the bound context
  it gave the calling plugin, never from the request, and keeps it when it
  relays a dynamic plugin's call in-process or over IPC. Only calls the host
  originates itself (for example the CLI) are owned by `bmux.core`. See
  [Calling services from dynamic plugins](../../docs/plugins.md#calling-services-from-dynamic-plugins).
- Use the context the host handed you: the `activate` context (and clones of
  it, for example in a background thread) works until deactivation begins,
  `deactivate`'s context works for that call, and a command's context works
  for that command. Calls through an expired context fail.
- Limits: 8 items per plugin, 64 total (`limit-exceeded` beyond that); ids are
  1..=64 bytes; text is trimmed, control characters become spaces, and text is
  truncated to 64 graphemes (tooltips 256). Toast titles are truncated to 80
  graphemes and bodies to 400.
- The host has no per-plugin unload signal, and state is in memory in the
  server process. Call `clear-status-items` from your plugin's `deactivate`.
- Toasts expire by wall-clock time. The provider keeps the newest 16, and the
  attach companion shows the newest `notifications.max_visible` in the top-right
  corner (below the bar when it sits on top), using the `toast_stack` component.
  `dismiss` removes one of your toasts early.
- Rendering: normal-severity items use the module style. Other severities tint
  it (info cyan, success green, warning yellow, critical bold red). Items appear
  left of the built-in mode/role/follow/hint modules, use at most half the bar,
  and are kept or dropped whole in priority order.

Delivery: the provider publishes retained `status-items-list` and
`notifications-list` snapshots (`bmux.tab_bar/status-items-list`,
`bmux.tab_bar/notifications-list`) on the plugin event bus. The server forwards
them to attach clients (`event_publications` in `plugin.toml`), and the attach
companion registers decoding channels for them. `examples/native-plugin`
(`status-set`, `status-clear`, `notify`) is a complete cross-dylib example.
