# bmux Theme Plugin

Owns runtime theme selection for bmux. Core configuration still declares the
startup theme with `[appearance].theme`; this plugin handles live selection,
preview, persistence, additive theme stacks, and generic theme-extension
fanout.

## Theme Stacks

BMUX accepts either a single theme or an ordered stack:

```toml
[appearance]
theme = "cyberpunk"
themes = ["cyberpunk", "mode-aware"]
```

When `themes` is present, themes are merged from left to right. Later layers
paint on top of earlier layers and may override or extend appearance fields,
mode-specific overlays, content effects, and plugin extension tables. Plugin
extension tables are deep-merged, so decoration component maps can be composed
by id across multiple theme files.

When only `theme` is set, BMUX treats it as the base theme and applies the
bundled `mode-aware` layer by default. `mode-aware` adds visible mode cues
without replacing the selected theme's visual identity.

## Plugin Extensions

Theme files can include plugin-owned extension tables. The theme plugin stores
and merges these tables generically; the owning plugin validates and interprets
the final value.

```toml
[plugins."bmux.decoration"]
script = "pulse"

[plugins."bmux.decoration".animation]
kind = "pulse"
hz = 30
```

This keeps the theme runtime domain-agnostic while allowing plugins such as
`bmux.decoration` to expose richer theme behavior. Users can also add final
component overrides in the `bmux.theme` plugin settings, without creating a
separate combination theme:

```toml
[plugins.settings."bmux.theme"]
themes = ["performance", "rainbow-snake"]

[plugins.settings."bmux.theme".components.snake]
above = ["performance.border"]
below = ["performance.header"]
```
