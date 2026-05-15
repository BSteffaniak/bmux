# bmux_decoration_plugin

The decoration plugin for BMUX. Owns all pane visual styling (borders,
focus highlighting, decoration characters, animated effects). Depends on
the windows plugin API for pane lifecycle state, and exposes its own
typed API via `bmux_decoration_plugin_api` for other plugins to query
and adjust the decoration style.

## Built-in styling

The plugin ships four built-in border styles (`none`, `ascii`, `single`,
`double`) plus a handful of bundled themes under `assets/themes/`
(`hacker`, `cyberpunk`, `minimal`, `pulse-demo`, `rainbow-snake`,
`performance`). ASCII is the default, matching the characters the core renderer
falls back to when no theme is active.

## Startup ready gate

By default, attach startup waits briefly for `bmux.decoration` to publish its
initial `scene-published` ready signal before drawing the first frame. Tune or
disable that gate with plugin settings:

```toml
[plugins.settings."bmux.decoration"]
startup_ready_gate = true
startup_ready_timeout_ms = 2000
```

Set `startup_ready_gate = false` to skip the wait entirely.

## Lua scripting (`decorate(message)`)

Themes can attach a Lua script that emits paint commands each animation
tick. The `scripting-luau` feature is enabled by default; consumers that
want a stub build can opt out via `default-features = false`.

Attach a legacy whole-theme script from your theme:

```toml
# ~/.config/bmux/themes/my-theme.toml
[plugins."bmux.decoration"]
script = "pulse"                      # bundled name OR filesystem path

[plugins."bmux.decoration".animation]
kind = "pulse"
hz   = 30                             # ticks per second; no upper clamp
```

Theme authors can also expose named, user-composable components. Users layer
components relative to other component ids rather than writing numeric z-indexes:

```toml
[plugins."bmux.decoration".components."performance.border"]
script = "performance_header"
entrypoint = "border"

[plugins."bmux.decoration".components."performance.header"]
script = "performance_header"
entrypoint = "header"
above = ["performance.border"]

[plugins."bmux.decoration".components.snake]
script = "rainbow_snake"
above = ["performance.border"]
below = ["performance.header"]
```

Later theme layers can disable or reorder individual components by id:

```toml
[plugins."bmux.decoration".components.snake]
below = ["performance.header"]

[plugins."bmux.decoration".components."performance.header"]
enabled = false
```

### Script resolution

The `script = "..."` value is resolved in this order:

1. An absolute path is read directly.
2. A relative path containing `/` or `.` is read relative to the user's
   config directory (`~/.config/bmux/` on Unix).
3. A bare stem (no slashes, no dots) matches a bundled script by name.
   The plugin ships `pulse` and `rainbow_snake`; see
   `assets/decorations/` for the sources.

### The `decorate(message)` contract

Scripts must define a global `decorate(message)` function. Render messages
return paint commands grouped by pane id:

```lua
function decorate(message)
    if message.kind ~= "render" then
        return nil
    end
    return { surfaces = { [message.panes[1].id] = {} } }
end
```

Render messages carry:

| Field             | Type       | Meaning                    |
| ----------------- | ---------- | -------------------------- |
| `message.kind`    | `"render"` | Message type               |
| `message.time_ms` | `u64`      | Ms since plugin activation |
| `message.frame`   | `u64`      | Monotonic frame counter    |
| `message.panes`   | `array`    | Visible pane snapshots     |

Each pane has `id`, `rect`, `content_rect`, `focused`, `zoomed`, and
`status`. Event messages use `message.kind = "event"` and carry
`message.event.source`, `kind`, `delivery`, `snapshot`, and `payload` so
scripts can cache plugin-defined signals.

Paint-command tables carry a `kind` string plus the variant fields; the
supported kinds are `text`, `filled_rect`, `gradient_run`, `box_border`, and
`semantic_border`. `semantic_border` is the ergonomic choice for adaptive
chrome: provide `fallback_glyphs`, `thickness_px`, and optional capability
predicates, and the renderer selects the best available path for the attached
terminal while preserving the glyph fallback.

```lua
table.insert(cmds, {
  kind = "semantic_border",
  rect = pane.rect,
  z = 10,
  fallback_glyphs = "thick",
  thickness_px = 3,
  radius_px = 2,
  style = { fg = bmux.rgb(255, 180, 90), bold = true },
})

-- Explicit advanced branch. The text renderer skips this unless Kitty
-- graphics and cell pixel metrics are known; pair it with an ungated or
-- inverse-gated fallback command for deterministic behavior everywhere.
table.insert(cmds, {
  kind = "semantic_border",
  rect = pane.rect,
  z = 10,
  fallback_glyphs = "thick",
  thickness_px = 3,
  when = { all = { "graphics-kitty", "cell-pixels" } },
  style = { fg = bmux.rgb(255, 180, 90) },
})
```

When `bmux_decoration_plugin_renderer` and the attach pipeline are built with
`image-kitty` and the attached terminal reports Kitty graphics, alpha support,
and cell pixel metrics, semantic borders emit pixel-backed border segments via
the attach renderer's generic terminal-graphics compositor. Sixel and iTerm2
inline images are intentionally not used for semantic borders because they do
not provide safe transparent overlay semantics for pane chrome. Unsupported
terminals deterministically fall back to the configured Unicode/text border
glyphs.
See `assets/decorations/pulse.lua` for a fully-worked example.

### `bmux.*` helper table

The sandbox injects a `bmux` global with:

- `bmux.log(level, msg)` — routed through the plugin's tracing bridge.
- `bmux.rgb(r, g, b)` — returns a color table shaped for the scene
  protocol's `Color::Rgb` variant.
- `bmux.named(name)` — named-palette color (e.g. `"bright_white"`).
- `bmux.hsl_to_rgb(h, s, l)` — standard HSL→RGB conversion returning
  a `(r, g, b)` tuple.
- `bmux.call_service(request)` — calls a declared plugin service and
  returns its decoded JSON-shaped response table.

### Declaring plugin data access

Decoration scripts do not receive broad plugin access by default. A theme
must declare the exact state channels, event channels, and service calls the
script may use under `[plugins."bmux.decoration".script_access]`:

```toml
[plugins."bmux.decoration"]
script = "my-decoration"

[plugins."bmux.decoration".script_access]
state_channels = ["example.metrics/pane-state"]
event_channels = ["example.metrics/pane-event"]

[[plugins."bmux.decoration".script_access.services]]
capability = "example.metrics.read"
kind = "query"
interface_id = "metrics"
operation = "pane"
```

State and event subscriptions are delivered to `decorate(message)` with
`message.kind == "event"`. The decoration plugin forwards the payloads as
opaque Lua tables; it does not interpret domain-specific data.

```lua
local latest = {}

function decorate(message)
    if message.kind == "event" then
        latest[message.event.source] = message.event.payload
        return nil
    end

    local pane = message.panes[1]
    local metrics = bmux.call_service({
        capability = "example.metrics.read",
        kind = "query",
        interface = "metrics",
        operation = "pane",
        payload = { pane_id = pane.id },
    })

    return { surfaces = { [pane.id] = {} } }
end
```

Service calls are denied unless `capability`, `kind`, `interface`, and
`operation` exactly match one of the declared grants. This keeps decoration
scripts generic and lets plugins own their own typed APIs.

### Sandbox

The mlua `StdLib` set is pared down to `STRING`, `MATH`, `TABLE`,
`UTF8`, and `COROUTINE`. `io`, `os`, `package`, `require`, `debug`, and
`dofile` are not reachable. The host `print` function is replaced by
`bmux.log`.

### Performance tracking

Each `decorate()` invocation is timed. A rolling P95 over the last 60
frames is compared against a soft threshold (8 ms by default); when the
threshold is crossed the plugin emits a `WARN` log at most once per
minute. There is no hard budget — users with expensive scripts own the
CPU cost.

## Try it

The `pulse-demo` bundled theme exercises the full scripting path.
Activate it through the `bmux.theme` plugin; no additional files are
required.

## Opting out

Scripting is on by default. The Luau backend is gated by the
`scripting-luau` feature on this crate (on in `default`) and by the
`decoration-scripting` feature on `bmux_cli` (on in its `default`,
which the `bmux` binary inherits automatically). To build a `bmux`
without the Luau dependency:

```
cargo build --bin bmux \
    --no-default-features \
    --features "bmux_cli/bundled-fonts bmux_cli/bundled-plugins bmux_cli/compression bmux_cli/kitty-keyboard bmux_cli/image-protocols"
```

The resulting binary falls back to a stub backend: themes that set
`script = "..."` log a warning at activation and render with their
static border/badge settings.
