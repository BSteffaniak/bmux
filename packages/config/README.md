# bmux_config

Configuration management for bmux terminal multiplexer.

## Overview

This package handles all configuration aspects of bmux, including file paths, themes, keybindings, and behavior settings.

## Features

- Configuration file loading and validation
- Default configuration generation
- Environment-specific settings
- Theme and keybinding management
- Path resolution utilities

## Configuration Structure

- **Behavior**: Terminal behavior settings
- **Appearance**: Visual themes and styling
- **Keybindings**: Custom key mappings
- **Paths**: File and directory locations

## Usage

```rust
use bmux_config::BmuxConfig;

let config = BmuxConfig::load_or_default()?;
```

## Explicit application composition

`ConfigFamily::capture(&[ConfigSource { path, required }])` reads only selected
files, in low-to-high precedence order. It records missing optional files and
retains parsed TOML and absolute origin paths in `ConfigDocument`. Callers may
also supply captured documents directly. Set `local = true` for scoped local
sources and supply a `ConfigScopeTarget` during resolution; ordinary documents
are unscoped. Capture does not create directories or read BMUX overrides.

`resolve_application_configuration(&bterm, &bmux, forced_profile, target)` uses
the same deep merge, array replacement, profile inheritance, auto-selection,
layer order, and scoped-local policy as standalone BMUX. Both families resolve
without defaults, then the BMUX family overlays the application's BMUX fields,
including profile effects. Defaults are inserted once after combining families.
The optional forced profile applies independently to both families, so both
must define that profile (or use a built-in profile).

The result exposes:

- `bmux`: typed, interpolated, repaired and validated BMUX configuration.
- `bmux_raw`: sparse, composition-free BMUX TOML for inherited snapshots.
- `startup_snapshot()`: complete typed defaults and normalized known fields,
  preserving unknown root/nested extension keys and table-array extensions.
  Prefer this over serializing `bmux` for private companion startup. It emits
  `[_bmux_startup_snapshot]` with `version = 1` and
  `representation = "resolved-paths"`. Ordinary loading validates this exact
  composition-free representation and skips only path interpolation (not
  repairs/validation), preventing a companion from expanding captured literal
  dollars, tildes, or unresolved tokens a second time. Unknown versions, extra
  marker fields, or composition controls fail closed with redacted diagnostics.
  This marker is a representation selector, not proof of trusted provenance.
  Original profile definitions remain in captured `sources`, not the snapshot.
- `application`: sparse resolved `[bterm]` data, without application defaults or
  schema validation. BMUX-family `[bterm]` data never affects this value.
- `application_family` and `bmux_family`: profile resolution plus winning leaf
  origins, keyed by full path segments (including `bterm` for application data).
- `sources`: both captured families, suitable as fingerprint inputs. These may
  contain sensitive values; they are not safe inspection output.

Known BMUX file paths are rebased against their winning source before snapshot
serialization. Application values remain untouched: consumers interpret their
own paths using `application_family.origins`, not the snapshot directory.
Built-in profile leaves have no file origin. An empty table has no leaf origin.

Resolution never discovers config paths or reads process config overrides.
Use `resolve_application_configuration_with_context(..., &context)` for
fully captured evaluation, where `ConfigEvaluationContext` contains
`cwd: Option<PathBuf>` and `environment: BTreeMap<String, OsString>`.
Missing variables/cwd never fall back to the process. `HOME` (or `USERPROFILE`
when absent) supplies tilde expansion; host/TERM/runtime and cwd profile rules
use the same captured inputs. Path tokens are expanded exactly once, without
printing variable names or values. Composition/type/validation failures are
redacted; malformed source diagnostics retain the selected source path only.
The compatibility `resolve_application_configuration` wrapper captures current
process evaluation inputs once; standalone BMUX loading retains its existing
interpolation behavior.

To intentionally admit host discovery,
call `capture_host_configuration(&paths, &overrides, scope)` separately; this
captures base, primary, mutable-state, environment/CLI-selected files and local
sources in ordinary precedence order, honoring `BMUX_NO_BASE_CONFIG` at capture.
Standalone BMUX loading APIs retain their existing behavior.
