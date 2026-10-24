# bmux Theme Plugin

Owns attachment-local theme selection for bmux. Declare the baseline theme in
`[plugins.settings."bmux.theme"]`; use the picker for a temporary per-view
selection and explicit default control to save a startup preference.

The performance settings capability is optional: themes without performance
settings do not require the performance plugin. Applying settings for an absent
provider returns an error rather than silently accepting an incomplete theme.
Decoration remains a required dependency for extension application and script
preview checkpoints.

When no explicit `default_theme` exists, legacy `selected_theme` preferences
are honored only with `persistence = "persist_between_connects"`. That legacy
preset takes precedence over `themes` and split `appearance_themes` /
`component_themes`; declared component overrides and targets still apply.
Otherwise, `declared_on_connect` uses the configured composition. Ordinary
picker confirmation never creates or updates either durable preference.

## Configured Selection and Stored Preferences

The picker includes **Use configured theme**, summarizing the appearance and
component stacks. It resolves the complete configured composition and bypasses
saved interactive provider settings without deleting them. Preset selections
remain distinct from this option, including presets with names resembling picker
control values.

Selection and preview are attachment-local, keyed only by authenticated
`caller_client_id`. Normal selection, confirmation, refresh, and component
settings never persist a preference or publish global appearance. Missing caller
identity is rejected for view mutations. Appearance queries return that caller's
effective preview/selection over its scoped configuration; unattached queries
inspect the baseline without applying decorations.

`theme-control-v1.current-default` and `select-default` inspect and explicitly
save the durable startup default. `reset-view` returns the caller to its latest
requested scoped baseline/default. Default changes affect new/reset views, not
existing views. The explicit `default_theme` preference uses the version-1 JSON
nullable `preset` representation and applies independently of the legacy
`persistence` setting. Legacy `selected_theme` records remain readable under
`persist_between_connects`, are not rewritten, and are superseded only when an
explicit default exists. A configured default preserves the complete declared
stack. Unsupported external providers without caller-scoped mutation contracts
are rejected rather than invoked globally.

Appearance snapshots retain the existing wire shape and are delivered only
through recipient-scoped retained transport. Unchanged appearances are not
serialized/published again; detach and reconciliation retire view state and
clear its retained appearance. Reconciliation uses neutral transport liveness,
so cleanup does not require the clients plugin: `Some(false)` retires a view;
unknown liveness preserves it. Global appearance forwarding is disabled.
Malformed or unsupported preference records are errors rather than implicit
configured-mode defaults; reading never rewrites configuration or storage.

The picker reads the theme owner's current selection and uses revision-checked
control services for selection, preview, confirmation, and cancellation. Each
attachment owns its own preview, independently of other attachments. Enter
confirms only that attachment's ephemeral selection; Esc removes its preview
overlay. Active pickers renew their preview lease. Detach retires that view;
lease expiry removes only its preview. Lua-backed decoration previews
retain a checkpoint of the original script instances, and cancellation restores
those instances rather than recompiling them. Failed rollback requires recovery
instead of being reported as a successful restoration.

The bundled picker captures a version-1 asynchronous service route before
spawning background work. Preview, cancellation, and confirmation use the
original attachment connection even after the command returns. The bounded
route closes on detach and never reconnects under a replacement identity.
Hosts without this route report the picker as unavailable rather than falling
back to unsafe preview ownership. This capture mechanism is currently for
bundled in-process commands, not a new dynamic-plugin wire ABI.

`bmux theme refresh` reloads the current selection's configuration and files.
It preserves the selection and does not write a reconnect preference. A failed
refresh leaves the retained selection/revision unchanged; rollback failures are
reported explicitly. Saving configuration or Lua files does not automatically
reload them. Bundled Lua assets require rebuilding the plugin to change them.

Daemon startup forwards the explicit `--config` overlay so refresh reads the
same highest-precedence file and reports malformed overrides. Saved interactive
provider values are restored for persisted presets across server restarts;
configured selection bypasses those values without deleting them.

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
