# Command Migration Matrix

This document classifies the current bmux CLI surface using the rule:

- core-native: bmux must retain this command natively for bootstrap, recovery, administration, or because native simplicity is preferable
- plugin-backed shipped: bmux can ship this command as a plugin while preserving the same user-facing CLI path
- plugin-backed later: a good plugin candidate, but not the first migration wave

Source of truth in code:

- `packages/cli/src/runtime/built_in_commands.rs`

## Permanent Core-Native

These commands are expected to remain native even if similar behavior could be implemented in plugins.

- `attach`
- `detach`
- `session attach`
- `session detach`
- `server`
- `server start`
- `server status`
- `server whoami-principal`
- `server save`
- `server restore`
- `server stop`
- `plugin`
- `plugin list`
- `plugin run`
- `terminal`
- `terminal install-terminfo`
- `keymap`
- top-level grouping roots like `session` and `window`

Rationale:

- required for bmux startup, recovery, or plugin administration
- operationally simpler and safer as native surfaces

## Migrate Soon: Plugin-Backed Shipped

These are the first shipped-plugin targets.

- `permissions`
- `grant`
- `revoke`
- `session permissions`
- `session grant`
- `session revoke`

Required host APIs:

- session read
- permission read
- client read
- event subscriptions for role/session/client changes

Rationale:

- bmux can function without them
- they are real user-facing features rather than bootstrap/admin paths
- they are a strong fit for command + event plugins

## Migrate Later: Plugin-Backed Candidates

These are good plugin candidates once host APIs deepen.

- `new-session`
- `list-sessions`
- `list-clients`
- `kill-session`
- `kill-all-sessions`
- `new-window`
- `list-windows`
- `kill-window`
- `kill-all-windows`
- `switch-window`
- `follow`
- `unfollow`
- `session new`
- `session list`
- `session clients`
- `session kill`
- `session kill-all`
- `session follow`
- `session unfollow`
- `window new`
- `window list`
- `window kill`
- `window kill-all`
- `window switch`
- `keymap doctor`
- `terminal doctor`

Likely additional host APIs needed:

- session write operations
- window write operations
- follow control operations
- richer pane/window/session DTOs

## Migration Order

1. `permissions`
2. `grant` / `revoke`
3. session/window/client list helpers
4. doctor-style diagnostics
5. window/session lifecycle convenience commands
6. follow/unfollow and more advanced workflows

## Notes

- Shipped vs external is a delivery concern, not an authority concern
- Command authority should come from capabilities, not plugin origin
- User-facing command paths should remain stable while implementation moves from native to shipped plugins
