# RuntimeAction domain-leak migration

This document tracks the remaining work to fully remove domain-specific
variants from `RuntimeAction` in `packages/keybind/src/lib.rs`, per the
AGENTS.md boundary rule that core architecture must stay
domain-agnostic.

## Background

`RuntimeAction` historically grew built-in variants for every
interactive bmux action (`NewWindow`, `FocusLeft`, `SplitFocusedVertical`,
`WindowGoto1..9`, etc.). These leak plugin-owned domain concepts
(windows, sessions, panes) into a core crate that ships alongside
`packages/server`, `packages/client`, and `packages/keybind`.

With the plugin triad (`bmux.windows`, `bmux.sessions`,
`bmux.pane-runtime`, `bmux.contexts`, `bmux.clients`) now owning their
domain state, these actions should be invoked via
`RuntimeAction::PluginCommand { plugin_id, command_name, args }` — the
single generic escape hatch — rather than hardcoded variants.

## Progress in this branch

- **SDK**: `PluginCommand` now carries an `accepts_repeat: bool`
  field (`packages/plugin-sdk/src/command.rs`). Plugins declare
  commands that should survive `KeyEventKind::Repeat`; core no longer
  needs to pattern-match on variant names.
- **Registry**: `PluginRegistry::command_accepts_repeat` (see
  `packages/plugin/src/registry.rs`) looks up a plugin command's
  repeat policy from the manifest.
- **Dispatch**: `action_supports_repeat` in
  `packages/cli/src/runtime/attach/runtime.rs` delegates to the
  registry for `RuntimeAction::PluginCommand`, via the
  `command_accepts_repeat` bridge in
  `packages/cli/src/runtime/plugin_runtime.rs`.
- **Windows-plugin commands**: `focus-pane-in-direction`, `split-pane`,
  `resize-pane`, `zoom-pane`, `close-active-pane`, `restart-pane` are
  now exposed as `[[commands]]` in
  `plugins/windows-plugin/plugin.toml` (previously service-handler
  only). Keybindings invoke them via `plugin:bmux.windows:<cmd>`.
  `focus-pane-in-direction` and `resize-pane` declare
  `accepts_repeat = true`.
- **Sessions-plugin commands**: `new-session` is now a declared
  `[[commands]]` entry in `plugins/sessions-plugin/plugin.toml` with
  a handler in `run_command`.
- **Windows-plugin keybindings**: default chords for pane focus /
  split / resize / zoom / close / restart are now `plugin:bmux.windows:*`
  strings in the plugin's own `[keybindings.runtime]`.

## Still to do

The following domain variants remain in `RuntimeAction` for backward
compatibility with the existing `default_runtime` keymap, profile
TOMLs (`tmux_compat.toml`, `zellij_compat.toml`), and the legacy
`handle_attach_runtime_action` / `handle_attach_ui_action` branches
that call into typed dispatch directly instead of going through the
plugin-command path.

Variants to delete once their behavior is fully migrated:

| Variant                             | Replacement                                                     | Blocker                                                                         |
| ----------------------------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `NewWindow`                         | `plugin:bmux.windows:new-window`                                | Default keymap still uses the bare name                                         |
| `NewSession`                        | `plugin:bmux.sessions:new-session`                              | Same                                                                            |
| `SessionPrev` / `SessionNext`       | `plugin:bmux.sessions:prev-session` / `next-session`            | sessions-plugin needs these commands + port of `switch_attach_session_relative` |
| `FocusNext/Prev/Left/Right/Up/Down` | `plugin:bmux.windows:focus-pane-in-direction --direction <dir>` | Default keymap + test rewrites                                                  |
| `SplitFocusedVertical/Horizontal`   | `plugin:bmux.windows:split-pane --direction <dir>`              | Same                                                                            |
| `IncreaseSplit/DecreaseSplit`       | `plugin:bmux.windows:resize-pane --direction increase/decrease` | Same                                                                            |
| `ResizeLeft/Right/Up/Down`          | `plugin:bmux.windows:resize-pane --direction <dir>`             | Same                                                                            |
| `ZoomPane`                          | `plugin:bmux.windows:zoom-pane`                                 | Same                                                                            |
| `CloseFocusedPane`                  | attach-runtime prompt → `plugin:bmux.windows:close-active-pane` | Confirm prompt still lives in core; OK                                          |
| `RestartFocusedPane`                | `plugin:bmux.windows:restart-pane`                              | Underlying pane-runtime primitive is a stub                                     |
| `ToggleSplitDirection`              | (no replacement; no-op today)                                   | Safe to delete outright                                                         |
| `EnterWindowMode` / `ExitMode`      | (no replacement; stub status message only)                      | Safe to delete outright                                                         |
| `WindowPrev/Next/Goto1..9/Close`    | `plugin:bmux.windows:{prev,next,goto,close-current}-window`     | Default keymap + tests only                                                     |

## Next PR outline

1. Rewrite `default_runtime` in `packages/cli/src/input/mod.rs:102-151`
   to use `plugin:bmux.*:*` strings for every DOMAIN-classified binding.
2. Rewrite `tmux_compat.toml` and `zellij_compat.toml` bindings
   similarly.
3. Rewrite `packages/config/src/keybind.rs` default construction to
   bypass `RuntimeAction::*` domain variants.
4. Port `switch_attach_session_relative` from
   `packages/cli/src/runtime/attach/runtime.rs:3413` into
   sessions-plugin, then add `prev-session` / `next-session`
   `[[commands]]` with handlers.
5. Delete domain variants from `RuntimeAction`. Fix the resulting
   compile errors in:
   - `packages/keybind/src/lib.rs::action_to_name`, `parse_action`,
     `action_to_config_name`.
   - `packages/cli/src/runtime/attach/runtime.rs` (~15 handler arms
     in `handle_attach_ui_action`, `is_attach_runtime_action`,
     `runtime_action_to_attach_event_action`, help-hint classifier).
   - `packages/cli/src/playbook/engine.rs:2359-2628` (~250 lines of
     domain pattern matches).
   - Tests throughout: ~30 cases.
6. Add an architecture guardrail test (see
   `packages/cli/tests/architecture_guardrails.rs::runtime_action_has_no_domain_variants`)
   that fails CI if a domain variant sneaks back in.

## Breaking-change notes

The migration is explicitly a clean break — there is no alias shim.
After the next PR, any user config that uses bare action names like
`new_window`, `focus_left_pane`, `close_focused_pane`, etc. will fail
to parse at keymap-load time (`packages/keybind/src/lib.rs::parse_action`
returns `bail!("unknown keymap action")`). Users must rewrite their
configs to use full `plugin:<id>:<cmd> [args]` strings.

Bundled default profiles (`packages/config/profiles/*.toml`) will be
updated in the same PR, so the break affects only hand-written user
configs.
