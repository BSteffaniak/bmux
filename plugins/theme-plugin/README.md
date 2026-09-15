# bmux Theme Plugin

Owns runtime theme selection for bmux. Declare the startup theme in
`[plugins.settings."bmux.theme"]`; this plugin handles live selection,
preview, persistence, additive theme stacks, and generic theme-extension
fanout.

The performance settings capability is optional: themes without performance
settings do not require the performance plugin. Applying settings for an absent
provider returns an error rather than silently accepting an incomplete theme.
Decoration remains a required dependency for extension application and script
preview checkpoints.

With `persistence = "persist_between_connects"`, a saved picker preset takes
precedence over both `themes` and split `appearance_themes` / `component_themes`
stacks on reconnect. The preset resolves the same way as a live picker selection;
declared component overrides and targets still apply. With the default
`declared_on_connect` policy, reconnect uses the configured composition instead.

## Configured Selection and Stored Preferences

The picker includes **Use configured theme**, summarizing the appearance and
component stacks. It resolves the complete configured composition and bypasses
saved interactive provider settings without deleting them. Preset selections
remain distinct from this option, including presets with names resembling picker
control values.

Selection writes store a version-1 JSON record at `selected_theme`, with an
explicit nullable `preset` field: `null` selects the configuration; a string
selects that preset. Existing UTF-8 preset-name records remain readable and are
replaced only on a subsequent confirmed, persisted selection. Reading does not
rewrite configuration or storage. Unsupported versions, missing fields, and
malformed records are errors rather than configured-mode defaults. Older BMUX
versions do not understand these records; downgrade requires restoring a legacy
preference from a backup rather than treating the new record as a preset name.

The picker reads the theme owner's current selection and uses revision-checked
control services for selection, preview, confirmation, and cancellation. One
attachment owns a preview at a time. Active pickers renew their preview lease;
detach or lease expiry restores the prior theme. Lua-backed decoration previews
retain a checkpoint of the original script instances, and cancellation restores
those instances rather than recompiling them. Failed rollback requires recovery
instead of being reported as a successful restoration.

**Known live-attachment limitation:** the picker currently spawns background
work after its caller-process command returns. The command-scoped transport
route does not survive that return, so subsequent remote service calls can use
different connection identities. Live preview cancellation can consequently
fail its ownership check and leave a preview active, blocking selection and
refresh until successful cleanup. Local and cross-plugin unit tests do not
establish safety for this path. Resolving this requires a context-bound,
attachment-lifetime route; a replaceable process-global fallback or relaxed
preview ownership checks would not preserve isolation.

`bmux theme refresh` reloads the current selection's configuration and files.
It preserves the selection and does not write a reconnect preference. A failed
refresh leaves the retained selection/revision unchanged; rollback failures are
reported explicitly. Saving configuration or Lua files does not automatically
reload them. Bundled Lua assets require rebuilding the plugin to change them.

Cross-process override provenance and external settings-provider consistency
still require end-to-end validation; do not assume those integration paths are
complete solely from the local control-service tests.

## Theme File Precedence

User `themes/*.toml` files override bundled presets with the same filename stem.
The first host configuration-directory candidate has priority over fallback
candidates. A malformed overriding file is reported with its path and excludes
that preset from the catalog rather than silently substituting the bundled
version. Other valid presets remain available.

## Theme Stacks

BMUX accepts either a single theme or an ordered stack:

```toml
[plugins.settings."bmux.theme"]
theme = "cyberpunk"
themes = ["cyberpunk", "mode-aware"]
```

When `themes` is present, themes are merged from left to right. Later layers
paint on top of earlier layers and may override or extend appearance fields,
mode-specific overlays, content effects, and plugin extension tables. Plugin
extension tables are deep-merged, so decoration component maps can be composed
by id across multiple theme files.

For stacks where the base appearance is functionally important, split visual
identity from component packs:

```toml
[plugins.settings."bmux.theme"]
appearance_themes = ["performance", "mode-aware"]
component_themes  = ["performance", "pong", "rainbow-snake"]
```

`appearance_themes` supplies runtime colors/status/mode appearance.
`component_themes` supplies theme settings and plugin-owned component
definitions. For `bmux.decoration`, appearance layers keep base border chrome
while stripping scripted components; component-only layers preserve component
maps, top-level scripts, script access, input, and animation hints without
letting fun component packs replace the base border chrome.

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
component overrides and generic pane targets in the `bmux.theme` plugin
settings, without creating a separate combination theme:

```toml
[plugins.settings."bmux.theme"]
themes = ["performance", "rainbow-snake"]

[plugins.settings."bmux.theme".components.snake]
above = ["performance.border"]
below = ["performance.header"]

[plugins.settings."bmux.theme".component_targets]
"performance.*" = { kind = "all-panes" }
"snake" = { kind = "focused-pane" }
"pong.*" = { kind = "unfocused-panes" }
```
