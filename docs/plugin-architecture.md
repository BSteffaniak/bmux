# Plugin Architecture

## Objectives

- Native-first performance for the initial plugin system
- Low-friction authoring for simple one-off plugins
- Deep integration points for advanced plugins
- Stable host-facing contracts that do not expose bmux internals directly
- Long-term ability to move more built-in functionality behind plugin-style interfaces

## Core Principles

1. Keep the public plugin contract in `bmux_plugin`
2. Do not make `packages/server`, `packages/cli`, or `packages/terminal` the public plugin API
3. Split plugin capabilities by depth and risk
4. Make hot-path runtime hooks explicit and rare
5. Version the plugin API and native ABI independently from the bmux release version

## Capability Tiers

### Automation

For simple plugins that should be easy to write and maintain.

- commands
- event subscriptions
- status bar items
- key actions
- storage
- clipboard

### Integration

For plugins that need to interact with bmux state in a controlled way.

- session read/write
- window read/write
- pane read/write
- attach overlays

### Runtime

For plugins that sit on hot terminal paths.

- terminal protocol observation
- terminal input interception
- terminal output interception

These hooks should remain opt-in, capability-gated, and narrow.

## Host Boundary

Plugins interact with bmux through service traits exposed by `bmux_plugin`.

- `PluginHost`
- `EventService`
- `CommandService`
- `SessionService`
- `WindowService`
- `PaneService`
- `RenderService`
- `ConfigService`
- `PluginStorage`
- `ClipboardService`

This keeps the host boundary explicit and lets bmux refactor internals without forcing every plugin to track those changes.

## Native Plugin Shape

The native-first model assumes plugins declare:

- a manifest
- plugin API compatibility
- native ABI compatibility
- requested capabilities
- an exported entry symbol

For Rust plugins, `bmux_plugin` should provide the default authoring path so plugin authors implement ordinary Rust traits and let the crate generate the native export glue.

The host should load a plugin only after validating compatibility and capability support.

## Why This Is Not Raw Internal Rust API

Raw internal types would make plugins brittle and would tightly couple every plugin to bmux internals. The better path is to expose narrow plugin-facing contracts owned by `bmux_plugin` and adapt core runtime code behind them.

## Planned Integration Sequence

### Phase 1

- manifest loading
- declaration validation
- capability negotiation
- plugin registry
- versioning policy

### Phase 2

- command contributions
- event fanout bridge
- plugin config access
- plugin storage
- status bar contributions
- keybind targets for plugins

### Phase 3

- host mutation APIs for session/window/pane operations

### Phase 4

- attach overlays
- protocol observation hooks
- constrained terminal interception hooks

### Phase 5

- migrate lightweight built-ins onto plugin-style contracts

## Refactors Still Needed In bmux

- command registration should stop being fully closed around the current CLI enum
- runtime key actions should support plugin-defined actions rather than only built-in enums
- server request handling should be decomposed into smaller units with clear extension seams
- plugin-facing event flow should converge on a single event model

## Command Boundary

bmux classifies commands using a simple rule:

- core-native: bmux must retain the functionality for bootstrap, recovery, administration, or native simplicity
- plugin-backed: bmux can still exist without the feature, so it is a candidate for shipped or installed plugins

The current migration matrix lives in `docs/command-migration-matrix.md`.
