# Config Profiles and Composition

BMUX supports full-config profile composition through the `[composition]` block.
Profiles can patch any config section (`general`, `behavior`, `keybindings`,
`plugins`, etc.), not just keymaps.

## Core Model

- `composition.active_profile`: selected profile id (optional).
- `composition.layer_order`: precedence order (optional).
- `composition.profiles.<id>.extends`: parent profiles (multiple supported).
- `composition.profiles.<id>.patch`: profile patch table (partial `BmuxConfig`).

Profile IDs are case-insensitive.

## Built-In Profiles

BMUX ships these profiles as normal profiles (not special-case code paths):

- `vim`
- `tmux_compat`
- `zellij_compat`

Built-in intent:

- `vim`: richer modal defaults (normal/insert/visual/command mode baseline)
- `tmux_compat`: tmux-like `ctrl+b` prefix flow and common pane/window keys
- `zellij_compat`: zellij-style global `alt+...` pane management layered on top of `vim`

Built-in patch content is sourced from regular TOML profile files in
`packages/config/profiles/` and merged through the same deep-merge logic as
user-defined profiles.

You can override or extend them the same way as user-defined profiles.

## Merge and Precedence Rules

- Deep merge is used for maps/tables.
- Scalars use last-writer-wins based on effective layer order.
- Arrays/lists use `replace` semantics.
- Multiple inheritance uses left-to-right parent application.
- When parents conflict, the **rightmost parent wins**.

Hard startup errors are raised for invalid composition input, including:

- unknown profiles
- unknown `layer_order` tokens
- inheritance cycles

## Layer Order Tokens

Supported `composition.layer_order` entries:

- `defaults`
- `config`
- `profile:active`
- `profile:<name>`

If `layer_order` is omitted:

- with `active_profile`: `defaults -> profile:active -> config`
- without `active_profile`: `defaults -> config`

## Example: Team Base + Personal Overlay

```toml
[composition]
active_profile = "dev_local"
layer_order = ["defaults", "profile:vim", "profile:team_base", "profile:active", "config"]

[composition.profiles.team_base]
extends = ["tmux_compat", "zellij_compat"]

[composition.profiles.team_base.patch.general]
server_timeout = 9000

[composition.profiles.team_base.patch.behavior]
pane_term = "xterm-256color"

[composition.profiles.team_base.patch.plugins]
enabled = ["bmux.windows", "bmux.permissions"]

[composition.profiles.dev_local]
extends = ["team_base"]

[composition.profiles.dev_local.patch.keybindings]
initial_mode = "normal"

[composition.profiles.dev_local.patch.keybindings.modes.insert.bindings]
escape = "enter_mode normal"

[composition.profiles.dev_local.patch.keybindings.modes.normal.bindings]
i = "enter_mode insert"

# Local file section still applies according to layer_order.
[general]
scrollback_limit = 15000
```

## Quick Preset: tmux

```toml
[composition]
active_profile = "tmux_compat"
```

This enables the built-in tmux-style profile (including `ctrl+b` prefix and
common pane/window keys) as your active profile.

## Quick Preset: zellij

```toml
[composition]
active_profile = "zellij_compat"
```

This enables the built-in zellij-style profile (which also layers on top of
`vim` defaults).

## Compose tmux + zellij + local overrides

```toml
[composition]
active_profile = "my_combo"

[composition.profiles.my_combo]
extends = ["tmux_compat", "zellij_compat"]

[composition.profiles.my_combo.patch.general]
server_timeout = 9000
```

Because parent application is left-to-right, `zellij_compat` (rightmost here)
wins on conflicting keys.

## Example: Rightmost Parent Wins

```toml
[composition]
active_profile = "child"

[composition.profiles.left.patch.general]
server_timeout = 100

[composition.profiles.right.patch.general]
server_timeout = 200

[composition.profiles.child]
extends = ["left", "right"]
```

Effective `general.server_timeout` is `200`.

## Keymap-Specific Notes

Modal keybindings are configured under `keybindings.modes`. Profile composition
can patch the whole keybinding block or only specific nested keys.

`enter_mode <mode_id>` targets are validated at startup. Invalid targets fail
startup with a configuration error.

## CLI Helpers

Profile composition commands:

- `bmux config profiles list`
- `bmux config profiles show <profile-id>`
- `bmux config profiles resolve [profile-id]`
- `bmux config profiles explain [profile-id]`
- `bmux config profiles diff <from> <to>`
- `bmux config profiles lint`
- `bmux config profiles evaluate`
- `bmux config profiles switch <profile-id>`
- `bmux config profiles switch <profile-id> --dry-run`

Keybinding explain command:

- `bmux keymap explain "ctrl+b %"`
- `bmux keymap explain "h" --mode normal`

Config file layering overrides:

- `BMUX_CONFIG=/path/extra.toml` adds an extra config layer on top of discovered config.
- `--config /path/extra.toml` adds another layer and takes precedence over `BMUX_CONFIG`.
- Relative paths in `BMUX_CONFIG` and `--config` resolve against the current working directory.
- Layers are merged deeply; arrays/lists are replaced by higher-precedence layers.
