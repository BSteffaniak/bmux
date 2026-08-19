# Pane content padding

The `bmux.pane_runtime` plugin can constrain and align terminal content inside a pane without changing the pane's outer layout rectangle. The pane's PTY, rendered terminal grid, cursor, mouse coordinates, and scrollback use the resulting content rectangle.

## Configuration

Configure defaults and ordered rules under the pane-runtime plugin settings:

```toml
[plugins.settings."bmux.pane_runtime".padding]
left = 0
right = 0
top = 0
bottom = 0
max_content_width = 120
horizontal_alignment = "center"
persist_runtime_overrides = true

[[plugins.settings."bmux.pane_runtime".padding.pane_rules]]
min_width = 180
max_content_width = 120
horizontal_alignment = "center"

[[plugins.settings."bmux.pane_runtime".padding.pane_rules]]
match_name = "logs-*"
match_command = "journal*"
left = 2
right = 2
max_content_width = "none"
```

All fields are optional. The compatibility default is zero padding, no maximum width or height, left horizontal alignment, top vertical alignment, and runtime-override persistence enabled.

Available default and rule values are:

- `left`, `right`, `top`, `bottom`: fixed padding in terminal cells.
- `max_content_width`, `max_content_height`: a positive cell count, or `"none"` in a rule to clear an inherited maximum.
- `horizontal_alignment`: `"left"`, `"center"`, or `"right"`.
- `vertical_alignment`: `"top"`, `"center"`, or `"bottom"`.
- `persist_runtime_overrides`: whether live overrides are included in pane-runtime snapshots.

Rules are evaluated in declaration order and the first matching rule wins. Matchers within one rule use AND semantics:

- `match_name`, `match_shell`, and `match_command` support `*` and `?` wildcards.
- `min_width`, `max_width`, `min_height`, and `max_height` are inclusive and inspect the pane's content dimensions before user padding is applied.

A rule inherits unspecified values from the global padding settings. A live runtime override is a complete specification and takes precedence over declarative settings and rules.

If fixed padding and limits do not fit, bmux clamps the content area deterministically to at least `1x1`. Center alignment places an odd surplus cell on the trailing side. Padding is pane-scoped and shared by every client attached to that pane because the pane has one canonical PTY size.

## Live configurator

Open the floating live-preview configurator from a CLI or attach plugin-command binding:

```sh
bmux pane-padding configure
```

```toml
[keybindings.runtime]
"Ctrl+A P" = "plugin:bmux.pane_runtime:pane-padding-configure"
```

The recommended chord is `Ctrl+A, P`. The modal previews edge padding, content limits, presets, and alignment immediately without changing pane outer rectangles or split ratios. `Esc` cancels and restores the underlying declarative/runtime state; `Enter` applies once.

Scopes are current pane, current window, current session, all open panes, and global default. Current-window scope requires the windows plugin; other scopes continue to work without it. Scope targets are frozen when selected, so panes opened afterward are not silently added.

Runtime-only changes disappear when the pane runtime is recreated. **Restore with pane** changes enter pane snapshots. **Global default** edits only `[plugins.settings."bmux.pane_runtime".padding]` in `bmux.toml`, preserves unrelated configuration, installs the defaults live for panes without overrides, and applies to future panes. Existing runtime or snapshot overrides retain precedence.

## Runtime commands

Inspect the focused pane in the current client's selected session:

```sh
bmux pane-padding show
```

Target another session by UUID or name and optionally a pane by UUID:

```sh
bmux pane-padding show --session work --pane-id <UUID>
```

Set a runtime override. A partial set starts from the pane's current effective specification:

```sh
bmux pane-padding set --max-content-width 120 --horizontal-alignment center
bmux pane-padding set --all 2 --left 4
bmux pane-padding set --max-content-width none
```

Precedence for edge options is `--all`, then `--horizontal`/`--vertical`, then individual edges. Reset returns the pane to its declarative default or matching rule:

```sh
bmux pane-padding reset
```

These are normal plugin commands, so keybindings can invoke the same command paths. For example:

```toml
[keybindings.runtime]
"Ctrl+Shift+P" = "plugin:bmux.pane_runtime:pane-padding-set --max-content-width 120 --horizontal-alignment center"
```

Plugin-command keybindings receive a concise status message and do not write command output into the attached terminal.

## Persistence and lifecycle

Runtime overrides are stored on the owning pane and use the normal pane-runtime snapshot. With `persist_runtime_overrides = true`, an override survives a bmux restart only when pane restoration is enabled with `behavior.restore_last_layout`. Disabling pane restoration means neither the pane nor its override is restored.

`pane-padding reset` removes the override from subsequent snapshots. Closing a pane or removing its session naturally removes the override; there is no separate padding state file or stale-record pruning mechanism.

## Superwide monitor example

This rule keeps ordinary panes full width and constrains only panes whose pre-padding content width reaches 180 cells:

```toml
[plugins.settings."bmux.pane_runtime".padding]

[[plugins.settings."bmux.pane_runtime".padding.pane_rules]]
min_width = 180
max_content_width = 120
horizontal_alignment = "center"
```

A full-screen pane on a superwide display retains its full outer rectangle while its terminal application receives a 120-column PTY centered between clear gutters. Resizing below 180 columns returns it to normal full-width behavior.
