# Bterm configuration

## Scope

Bterm is a native frontend for BMUX, not a second execution or plugin manager.
BMUX plugins continue to own commands, tabs, splits, workspaces, execution, and
permissions. Native window configuration must not create or reorder those
resources.

Configuration inspection describes selected inputs and policy. It does not
establish that a companion launched, an attachment succeeded, or native
decorations rendered correctly.

## Configuration sources

The Bterm file is a superset: it accepts the BMUX configuration alongside
Bterm-owned native window settings. A separate BMUX configuration file is not a
prerequisite for a self-contained Bterm configuration.

There are two source families:

- **BMUX:** host `BMUX_CONFIG_DIR/bmux.toml`, then the `BMUX_CONFIG` file overlay,
  then `BMUX_CONFIG_TOML` inline TOML, then `--bmux-config PATH`.
- **Bterm:** host `BTERM_CONFIG_DIR/bterm.toml`, then the `BTERM_CONFIG` file overlay,
  then `BTERM_CONFIG_TOML` inline TOML, then `--config PATH`.

Within each family, the implemented source order is base, primary, mutable
state,
environment file, environment inline TOML, CLI overlay, then explicitly selected
scoped-local files.
Inline text is captured once under the same byte/document bounds as files. Empty
text is a present empty document, not an absent source. Diagnostics never echo
its contents; inspection identifies its named origin instead of inventing a
file.
Relative paths in inline documents use the captured working directory. Explicit
managed roots, slots, and clean sandbox captures do not import ambient inline
variables. A private effective snapshot can retain already-selected values; that
is not authority to rediscover the original host environment on reload.
Later supplied fields replace earlier fields; absent fields remain inherited.
Bterm's mutable state file is `config-overlay.toml` in its selected state
directory.
`BTERM_BASE_CONFIG`/`BMUX_BASE_CONFIG` select base inputs; the corresponding
`*_NO_BASE_CONFIG` switch suppresses the environment-selected base input.

Resolve family composition and profiles independently, then apply the BMUX
family over the BMUX fields of the Bterm superset. Only the Bterm family
supplies
`[bterm]` native settings; `[bterm]` in BMUX inputs is ignored. Thus a partial
BMUX
overlay replaces its supplied BMUX fields without erasing other superset fields.
Do not deserialize each file with defaults
and then merge: that would let an omitted field erase a supplied value. An
explicit `false`, `0`, or empty collection is a value, not absence. Defaults
belong
after family/profile composition. Profile composition uses BMUX's existing
`[composition]` model, including `active_profile`, profile inheritance,
auto-selection, and `layer_order`. A forced profile applies independently to
both families. Profile selection and diagnostic fields are separate from
arbitrary configuration values.

An explicitly selected overlay that is missing, unreadable, or malformed must
fail. It must not silently become an optional source or fall back to host state.
Source-relative paths must retain their source context through composition.

`--no-bmux-config` disables ordinary host BMUX configuration inheritance; it
does
not disable BMUX itself or erase BMUX settings provided in the Bterm file.

## Superset file example

The native configuration type currently accepts `[bterm.window].decorations`.
The shared resolver also accepts ordinary BMUX fields in the same document:

```toml
[general]
scrollback_limit = 5000

[bterm.window]
decorations = false
```

This file controls new windows, not an existing window's native chrome.
An inherited BMUX `general.scrollback_limit` overrides
that field; use the no-host-inheritance policy for a self-contained superset.
Unknown native settings are rejected rather than silently ignored.

## Managed and external runtime policy

`--managed-root ROOT` selects a complete persistent root and is self-contained
by default. Ambient host configuration must not leak into an explicitly selected
root. Configuration inheritance is a separate authority decision from selecting
a companion executable. Passing a root does not authorize adopting another
runtime, rewriting host configuration, or stopping a running companion.

Managed runtime transports use `run/<kernel-boot-uuid>/` below the root on Linux
and macOS; `config/`, `data/`, and `state/` remain durable. Runtime socket paths
are length-limited, so use a short absolute root. Existing
lifetime/compatibility
checks still apply; configuration must not bypass them.

`--runtime-root ROOT` selects an external runtime, attach-only. Its explicit
`runtime/` layout is not migrated or repaired. Configuration inspection for an
external root must not require a bundled companion, contact its endpoint, or
start a replacement runtime. Closing a window detaches; it does not stop
executions or the companion.

## Read-only inspection

Inspect configuration with:

```sh
bterm config explain
```

It must resolve the same captured configuration inputs used by startup and emit
JSON without opening a window, initializing a renderer, connecting to an
installed runtime, or starting a companion. Inspection must not create root
folders, sockets, locks, snapshots, configuration files, or default files.
Missing selected inputs and invalid execution contexts must remain errors even
in inspection mode.

The successful JSON output is a diagnostic summary, **not a configuration
dump**.
It includes a configuration generation, native window settings, source
provenance,
a redacted BMUX marker, and an `execution` object. Execution mode is `managed`,
`external`, `slot-attach`, or `sandbox-attach`, with selected configuration,
runtime, data, and
state paths. Arbitrary configuration values and environment contents are not
part
of this JSON summary.

Malformed TOML diagnostics identify the selected source without reproducing its
contents. Paths and profile identifiers in successful output may still be
sensitive; review diagnostics before sharing them.

An explanation is not evidence of successful runtime startup.

## Explicit slot attachment

`bterm --slot NAME --slots-manifest /absolute/path/slots.toml` selects only that
manifest's named slot and attaches to its endpoint. It never launches a managed
companion, adopts an ambient slot selector, replaces an endpoint, or falls back
to ordinary host roots. Both flags are required together and conflict with
explicit runtime, companion, managed-root and sandbox selection. Config/state,
data, logs, and runtime locations come from the selected slot; its declared base
inheritance is preserved. The selected binary must exist and be executable, but
is not executed. The existing slot resolver is evaluated during startup against
the captured process environment; a changed/sanitized snapshot is rejected.
Inspection reports these roots without creating state or contacting the server.

## Reload behavior

An explicit reload through the existing attach path rereads the captured source
selection rather than discovering new host sources. It can apply native
typography
and zoom settings to the existing window, retaining its current zoom target
under
the new bounds. BMUX/runtime configuration, companion policy, and creation-time
window changes are refused and require an explicit restart. There is no
automatic
file watcher or live reload merely from editing TOML. Frozen sandbox/effective
snapshots remain frozen; reload does not rediscover their source configuration.

## Sandbox handoff and state isolation

Sandbox execution is attach-only and requires a validated descriptor, selected
with `--execution-context PATH` or `BTERM_EXECUTION_CONTEXT`. Bterm does not
start
or recover a companion from the descriptor's BMUX executable. Explicit runtime
roots, companion selection, and CLI configuration overlays conflict with this
handoff; new overlays require a new sandbox snapshot.

A BMUX sandbox may supply a captured application execution context. Recognition
must happen before stripping environment selectors or selecting Bterm defaults.
A valid handoff supplies explicit configuration, runtime, data, state,
temporary,
and home paths, plus a validated BMUX artifact identity. That artifact is not a
Bterm executable and is not automatically a verified Bterm companion bundle.

Clean sandbox configuration and a captured development snapshot are distinct
policies. A snapshot must be used as captured, not rediscovered from the host.
Partial selectors, a missing or malformed descriptor, unsupported versions,
changed snapshot/artifact identities, or conflicting explicit roots must fail
closed. They must not retry against the ordinary user runtime or configuration.
A borrowed sandbox root does not confer cleanup or termination authority.

Child environments must be constructed from the captured policy rather than
merging ambient BMUX/Bterm selectors back in. Host configuration and state paths
must remain untouched.

**Isolation here is execution/configuration namespace isolation, not an OS
security sandbox.** It is not a filesystem access-control boundary and does not
protect the host from arbitrary shell commands. Handoff consistency checks do
not authenticate a malicious owner of the sandbox directory. OS confinement,
where needed, requires a separate security mechanism.

## Typography and zoom

Typography and zoom are Bterm presentation policy, not BMUX resource or
companion configuration. Whole-window zoom remains the default. Pane-scoped
controls change content typography inside the existing pane allocation; they
do not change split ratios, decorations, or federated arrangements. Pinch
gestures are not supported.

```toml
[bterm.typography]
size = 16.0
# family = "Your installed monospace family"
fallback = []
line_height = 1.0
letter_spacing = 0.0

[bterm.zoom]
step = 1.1
min_scale = 0.5
max_scale = 3.0
remember = true

[bterm.zoom.animation]
enabled = true
reduced_motion = false
duration_ms = 120
easing = "ease_out"

[bterm.zoom.wheel]
enabled = true
modifiers = ["primary"]
sensitivity = 0.1
```

Bterm captures these settings at startup and does not watch files. An explicit
reload through the existing attach path can update typography, bindings, wheel
policy, and animation in the existing window; it is not an automatic reload on
file edits. See [Reload behavior](#reload-behavior) for refused runtime/window
changes and frozen-snapshot behavior. Zoom shortcuts change window-local target
state independently of declarative configuration.

`size` is a logical-pixel font size, including fractional values such as
`15.25`;
it is not a point size or framebuffer-pixel size. Zoom multiplies the logical
size; monitor DPI is applied separately. Fractional sizes are rerasterized, not
rounded to integer font sizes or implemented by stretching an old glyph bitmap.
`line_height` multiplies natural line metrics, and `letter_spacing` adds logical
pixels per terminal column, not per glyph. Both participate in the same scaled
geometry used for text, cursor placement, images, and pointer mapping. Raster
cache sizes may use subpixel buckets (currently 1/64 physical pixel); that is
not
integer font-size stepping, and layout geometry remains continuous.

Logical letter spacing scales with the effective zoomed font-size ratio,
including
DPI-induced font caps; line height remains a dimensionless multiplier.

An omitted `family` uses bundled primary faces. `fallback` specifies ordered
installed families before remaining platform fallback. Missing installed
families
must not make a configuration depend on another machine's font availability;
exact glyph coverage and appearance still depend on the selected fonts. Family
names cannot be blank, contain control characters, or exceed 256 bytes; at most
16 fallback names are accepted. Configuration accepts size `4..128`, line height
`1..4`, and letter spacing `0..32`, all finite. Renderer raster-size limits also
apply after zoom and DPI: the window integration clamps effective logical font
size so both logical and physical raster sizes stay within `1..256`. The
requested
zoom target is retained, so extreme settings can display a capped size rather
than the exact requested size. Moving to another DPI can change that cap.
A separate capacity guard keeps the projected window grid at or below 100,000
cells. If a tiny zoom would exceed that budget, effective font size and spacing
are enlarged proportionally while the requested zoom target remains unchanged.
The applied policy is cached so a settled capacity-clamped window does not
continuously invalidate output. Reset returns to the configured base size,
subject to the same raster and capacity limits.

### Pane-scoped controls

This implementation is not yet end-to-end accepted. Captured-scrollback zoom
uses immutable capture-bound rows, palette and images, with source-anchored
selection; missing or mismatched captures reject presentation rather than
showing
a different source. Transformed accessibility and IME geometry is prepared and
committed with output. A real-daemon two-pane/two-view fixture verifies local
zoom and failed-output authority preservation, but does not exercise a native
Bterm window. Real-application visual acceptance, native IME/screen-reader
checks, mixed-DPI validation, and Linux native validation remain outstanding.

Pane zoom ships with primary-Alt-plus/minus/reset keyboard shortcuts for the
focused pane and primary-Alt-wheel zoom for the pane under the pointer.
`primary`
is Command on macOS and Control elsewhere; Alt is Option on macOS. Whole-window
primary-plus/minus/reset shortcuts remain unchanged. Plus accepts `=` and `+`,
including the Shift-`+` variant, for both scopes.

Set `scope = "pane"` on custom keyboard bindings or wheel policy to select pane
content rather than the whole window. Omitted keyboard binding scopes remain
window-scoped for compatibility. For example:

```toml
[bterm.zoom]
bindings = [
  { key = "=", modifiers = ["primary"], action = { action = "in" } },
  { key = "-", modifiers = ["primary"], action = { action = "out" } },
  { key = "=", modifiers = ["primary", "alt"], scope = "pane", action = { action = "in" } },
  { key = "-", modifiers = ["primary", "alt"], scope = "pane", action = { action = "out" } },
  { key = "0", modifiers = ["primary", "alt"], scope = "pane", action = { action = "reset" } },
]

[bterm.zoom.wheel]
scope = "pane"
modifiers = ["primary", "alt"]
```

Keyboard actions select the committed focused pane. Wheel actions select the
pane under the pointer without changing focus. Reset removes that pane's extra
scale, preserving whole-window zoom. The effective content size is the base
logical size multiplied by window and pane scales; display DPI remains separate.
Duplicate key/modifier combinations are invalid even when their scopes differ.

Normal-screen content reflows from structured source rows in the local view.
Zoom does not resize the shared application's PTY or make the application format
its output for the local width. Alternate-screen applications instead use the
existing authorized execution-resize path. Native alternate-screen sizing keeps
its existing last-writer policy: simultaneous views can resize the same
application; pane zoom does not create exclusive resize ownership. Bterm also
reconciles a saved pane scale when displayed content enters the alternate
screen or its allocation changes, so zooming before launching a full-screen
application uses the same sizing path as zooming inside it. Reconciliation is
bounded and stops after convergence; it does not continuously fight another
attachment's sizing. Rejected or timed-out requests retain zoom intent and emit
a diagnostic rather than replaying unacknowledged resizes.

For deliberate restoration, `bterm --view-binding work-left` names a local
preference namespace. Restoration still requires a matching currently committed
pane/source binding under the selected runtime and configuration. This label is
not an execution capability. Windows without a view binding retain independent,
ephemeral pane preferences. Reusing a label deliberately shares the saved
namespace, not live zoom state or input authority.

### Keyboard, wheel, and exact sizes

`primary` means Command on macOS and Control elsewhere. Default shortcuts are
primary-plus (`=` or `+`, including primary-Shift-`+`), primary-minus, and
primary-`0` to reset. Reset means scale `1.0`, returning to the configured base
size, not a hard-coded 16-pixel font. Steps multiply the target by `step` rather
than adding an integer to the font size. Scales are clamped to the configured
bounds, which must include `1.0`.

Replace the binding list to customize shortcuts. Actions use a tagged table;
`set_size` takes an exact logical size, while `set_scale` takes a ratio:

```toml
[bterm.zoom]
bindings = [
  { key = "=", modifiers = ["primary"], action = { action = "in" } },
  { key = "-", modifiers = ["primary"], action = { action = "out" } },
  { key = "0", modifiers = ["primary"], action = { action = "reset" } },
  { key = "s", modifiers = ["primary"], action = { action = "set_size", value = 17.75 } },
  { key = "z", modifiers = ["primary"], action = { action = "set_scale", value = 1.25 } },
]
```

Keys are single logical characters; modifier lists match exactly. Supported
modifiers are `primary`, `control`, `alt`, `shift`, and `super`. At most 64
bindings are accepted; duplicate effective key/modifier combinations are errors.
An empty binding list disables keyboard zoom. A matched press, its repeats, and
its release belong to zoom and must not also reach terminal input, even if the
modifier is released before the key. Focus loss clears held shortcut state.
Unmatched input retains its ordinary BMUX route.

Wheel zoom requires primary-Alt by default, targets the hovered pane, and is
enabled. Primary-only and unmodified scrolling stay on their ordinary routes.
To restore window wheel zoom, set `wheel.scope = "window"` and
`wheel.modifiers = ["primary"]`. One wheel
line equals 40 pixels; normalization clamps each event to 32 lines in either
direction. The normalized amount times `sensitivity` is a fractional step count.
Horizontal-only or non-finite vertical deltas do not change scale. Set
`wheel.enabled = false` to leave wheel input on its ordinary route. No pinch
recognition is implied by pixel-wheel support.

### Animation, remembered targets, and presentation authority

Animation defaults to 120 ms with `ease_out`; `linear` and `ease_in_out` are
also
available. Interpolation is logarithmic in scale. Retargeting starts from the
current animated sample, avoiding a jump back to the previous target. Animation
stops requesting frames at completion. Set `enabled = false`, `duration_ms = 0`,
or `reduced_motion = true` for immediate changes. Reduced motion is an explicit
configuration choice, not a claim of automatic OS accessibility detection.

`remember = true` restores the last requested target, not a mid-animation
sample.
Preferences are separate from declarative TOML, under the selected Bterm state
root in `window-zoom-preferences.json`. Their identity includes ordered selected
configuration source locators, profile/scope selectors, and the selected stable
runtime target. Native typography edits do not themselves change that identity
or the companion's runtime configuration identity. Different selected runtime
targets do not share a preference entry. Windows sharing an entry use the last
successfully saved target; in-flight animation and held keys remain
window-local.
Restored targets are clamped to current limits; `remember = false` starts at
`1.0`.
The preference format accepts any positive finite target; current zoom bounds
are applied by the controller, not encoded as a preference-format size limit.
Corrupt or unsupported preference data is reported; the window starts from the
default target with saving disabled for that window, preserving the original
file. Native-config identity stability does not imply automatic live reload;
use the explicit attach reload path for native-only changes. Saves are debounced
by 250 ms and performed on a bounded worker, with up to three retries after
failure. Close waits at most 200 ms for the worker/final target and reports a
missed deadline; last-target persistence is not guaranteed after that deadline.

A target or animation sample is not proof of output. Painting and interaction
must use the geometry from the same successfully presented output; failed or
pending changes cannot advance hit testing independently. Configuration
inspection and headless acceptance tests do not establish native GPU output,
OS shortcut delivery, persistence-worker scheduling, or visible animation
smoothness. See [the performance protocol](bterm-performance.md) for measurement
boundaries and native acceptance checks.

## Native decorations

The native platform's creation-time `WindowOptions::decorations` defaults to
`true`; `false` requests no native titlebar/chrome. This is native window
policy,
not BMUX tab-bar or terminal presentation configuration. Removing chrome also
removes its titlebar controls and drag affordance. It does not change plugin
resource authority. Existing windows do not restyle through a live setter.

The platform forwards the request to its native window backend on macOS and
Linux; a window manager/compositor may affect the visible result. Configuration
inspection reports the requested policy, not the compositor's visible result.
