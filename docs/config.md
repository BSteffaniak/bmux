# Configuration Reference

bmux is configured via a `bmux.toml` file. If no config file exists, bmux uses
sensible defaults for all options.

## Config File Location

bmux looks for `bmux.toml` in the standard XDG config directory:

```
~/.config/bmux/bmux.toml
```

You can also specify a custom path via the `BMUX_CONFIG` environment variable.

To see where bmux is looking for its config:

```sh
bmux config path
```

---

## `[general]`

General session and server settings.

| Option             | Type    | Default          | Description                                                                                   |
| ------------------ | ------- | ---------------- | --------------------------------------------------------------------------------------------- |
| `default_mode`     | string  | `"normal"`       | Default interaction mode when starting bmux. One of: `normal`, `insert`, `visual`, `command`. |
| `mouse_support`    | bool    | `true`           | Enable mouse support for pane selection, scrolling, and resizing.                             |
| `default_shell`    | string  | _(system shell)_ | Default shell to launch in new panes. When unset, uses `$SHELL` or `/bin/sh`.                 |
| `scrollback_limit` | integer | `10000`          | Maximum number of scrollback lines retained per pane. Must be at least 1.                     |
| `server_timeout`   | integer | `5000`           | Server socket timeout in milliseconds. Must be at least 1.                                    |

```toml
[general]
default_mode = "normal"
mouse_support = true
default_shell = "/bin/zsh"
scrollback_limit = 50000
server_timeout = 5000
```

---

## `[appearance]`

Visual theming and layout options.

| Option                | Type   | Default    | Description                                                                        |
| --------------------- | ------ | ---------- | ---------------------------------------------------------------------------------- |
| `theme`               | string | `""`       | Theme name to load.                                                                |
| `status_position`     | string | `"BOTTOM"` | Where to display the status bar. One of: `TOP`, `BOTTOM`, `OFF`.                   |
| `pane_border_style`   | string | `"SINGLE"` | Pane border drawing style. One of: `SINGLE`, `DOUBLE`, `ROUNDED`, `THICK`, `NONE`. |
| `show_pane_titles`    | bool   | `false`    | Show title labels on each pane.                                                    |
| `window_title_format` | string | `""`       | Format string for the terminal window title.                                       |

```toml
[appearance]
status_position = "BOTTOM"
pane_border_style = "ROUNDED"
show_pane_titles = true
```

---

## `[behavior]`

Terminal behavior, protocol handling, and runtime options.

| Option                          | Type    | Default           | Description                                                                                                                                             |
| ------------------------------- | ------- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `aggressive_resize`             | bool    | `false`           | Aggressively resize windows when clients disconnect.                                                                                                    |
| `visual_activity`               | bool    | `false`           | Show visual activity indicators for background panes.                                                                                                   |
| `bell_action`                   | string  | `"ANY"`           | Bell notification behavior. One of: `NONE`, `ANY`, `CURRENT`, `OTHER`.                                                                                  |
| `automatic_rename`              | bool    | `false`           | Automatically rename windows based on the running command.                                                                                              |
| `exit_empty`                    | bool    | `false`           | Exit bmux when no sessions remain.                                                                                                                      |
| `restore_last_layout`           | bool    | `true`            | Restore and persist the last local CLI runtime layout across sessions.                                                                                  |
| `confirm_quit_destroy`          | bool    | `true`            | Confirm before a destructive quit that clears persisted local runtime state.                                                                            |
| `pane_term`                     | string  | `"bmux-256color"` | Terminal type exposed to pane processes as `TERM`. Common values: `bmux-256color`, `xterm-256color`, `screen-256color`.                                 |
| `protocol_trace_enabled`        | bool    | `false`           | Enable protocol query/reply tracing in the runtime. Useful for debugging terminal protocol behavior.                                                    |
| `protocol_trace_capacity`       | integer | `200`             | Maximum number of in-memory protocol trace events to retain. Must be at least 1.                                                                        |
| `terminfo_auto_install`         | string  | `"never"`         | Auto-install policy for the bmux terminfo entry when missing. One of: `ask`, `always`, `never`.                                                         |
| `terminfo_prompt_cooldown_days` | integer | `7`               | Number of days to wait before prompting again after declining terminfo installation.                                                                    |
| `stale_build_action`            | string  | `"error"`         | Behavior when the running server build differs from the current CLI build. `error` blocks commands; `warn` allows them with a warning.                  |
| `kitty_keyboard`                | bool    | `true`            | Enable the Kitty keyboard protocol for enhanced key reporting. Allows modified special keys like Ctrl+Enter to be correctly forwarded to pane programs. |

```toml
[behavior]
pane_term = "bmux-256color"
restore_last_layout = true
confirm_quit_destroy = true
kitty_keyboard = true
stale_build_action = "error"
terminfo_auto_install = "ask"
```

---

## `[multi_client]`

Multi-client session sharing settings.

| Option                    | Type    | Default | Description                                                                                              |
| ------------------------- | ------- | ------- | -------------------------------------------------------------------------------------------------------- |
| `allow_independent_views` | bool    | `false` | Allow clients to have independent views of the same session (different focused panes, scroll positions). |
| `default_follow_mode`     | bool    | `false` | New clients automatically follow the leader client's view by default.                                    |
| `max_clients_per_session` | integer | `0`     | Maximum number of clients that can attach to a single session. `0` means unlimited.                      |
| `sync_client_modes`       | bool    | `false` | Synchronize interaction modes across all attached clients.                                               |

```toml
[multi_client]
allow_independent_views = true
default_follow_mode = false
max_clients_per_session = 0
```

---

## `[keybindings]`

Key binding configuration. bmux uses a prefix-key model (like tmux/screen) combined
with modal keybindings.

### Core Settings

| Option            | Type    | Default    | Description                                                                                                   |
| ----------------- | ------- | ---------- | ------------------------------------------------------------------------------------------------------------- |
| `prefix`          | string  | `"ctrl+a"` | Prefix key for runtime key chords. All prefixed bindings require pressing this key first.                     |
| `timeout_ms`      | integer | _(unset)_  | Exact timeout in milliseconds for multi-stroke chord resolution. Overrides `timeout_profile`. Range: 50–5000. |
| `timeout_profile` | string  | _(unset)_  | Named timeout profile to use. See built-in profiles below.                                                    |

When neither `timeout_ms` nor `timeout_profile` is set, bmux uses **indefinite
timeout** — it waits for the next key without a deadline (press Escape to cancel a
partial chord).

### Built-in Timeout Profiles

| Profile       | Timeout |
| ------------- | ------- |
| `fast`        | 200ms   |
| `traditional` | 400ms   |
| `slow`        | 800ms   |

You can override built-in profiles or define custom ones:

```toml
[keybindings]
timeout_profile = "traditional"

[keybindings.timeout_profiles]
traditional = 500    # override built-in
custom = 300         # define new profile
```

### Binding Scopes

Keybindings are organized into scopes:

| Scope     | Description                                       |
| --------- | ------------------------------------------------- |
| `runtime` | Bindings triggered after pressing the prefix key. |
| `global`  | Bindings that work without the prefix key.        |
| `scroll`  | Bindings active in scrollback/copy mode.          |
| `normal`  | Modal bindings for Normal mode.                   |
| `insert`  | Modal bindings for Insert mode.                   |
| `visual`  | Modal bindings for Visual mode.                   |
| `command` | Modal bindings for Command mode.                  |

### Default Runtime Bindings (after prefix)

| Key                             | Action                   |
| ------------------------------- | ------------------------ |
| `shift+c`                       | New session              |
| `o`                             | Focus next pane          |
| `h` / `arrow_left`              | Focus left               |
| `l` / `arrow_right`             | Focus right              |
| `k` / `arrow_up`                | Focus up                 |
| `j` / `arrow_down`              | Focus down               |
| `t`                             | Toggle split direction   |
| `%`                             | Split focused vertical   |
| `"`                             | Split focused horizontal |
| `+`                             | Increase split size      |
| `-`                             | Decrease split size      |
| `shift+h` / `shift+arrow_left`  | Resize left              |
| `shift+l` / `shift+arrow_right` | Resize right             |
| `shift+k` / `shift+arrow_up`    | Resize up                |
| `shift+j` / `shift+arrow_down`  | Resize down              |
| `r`                             | Restart focused pane     |
| `x`                             | Close focused pane       |
| `?`                             | Show help overlay        |
| `[`                             | Enter scroll/copy mode   |
| `]`                             | Exit scroll/copy mode    |
| `ctrl+y`                        | Scroll up one line       |
| `ctrl+e`                        | Scroll down one line     |
| `page_up`                       | Scroll up one page       |
| `page_down`                     | Scroll down one page     |
| `g`                             | Scroll to top            |
| `shift+g`                       | Scroll to bottom         |
| `v`                             | Begin selection          |
| `d`                             | Detach from session      |
| `q`                             | Quit                     |

### Default Scroll Mode Bindings

| Key                 | Action                              |
| ------------------- | ----------------------------------- |
| `escape`            | Exit scroll mode                    |
| `enter`             | Confirm scrollback (copy selection) |
| `h` / `arrow_left`  | Move cursor left                    |
| `l` / `arrow_right` | Move cursor right                   |
| `k` / `arrow_up`    | Move cursor up                      |
| `j` / `arrow_down`  | Move cursor down                    |
| `ctrl+y`            | Scroll up one line                  |
| `ctrl+e`            | Scroll down one line                |
| `page_up`           | Scroll up one page                  |
| `page_down`         | Scroll down one page                |
| `g`                 | Scroll to top                       |
| `shift+g`           | Scroll to bottom                    |
| `v`                 | Begin selection                     |

### Custom Bindings Example

```toml
[keybindings]
prefix = "ctrl+b"
timeout_profile = "fast"

[keybindings.runtime]
"shift+c" = "new_session"
"d" = "detach"

[keybindings.global]
"alt+h" = "focus_left"
"alt+l" = "focus_right"
```

---

## `[plugins]`

Plugin management settings.

| Option         | Type            | Default | Description                                                                                                                                  |
| -------------- | --------------- | ------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `enabled`      | list of strings | `[]`    | Plugins to explicitly enable. Bundled plugins (like `bmux.windows` and `bmux.permissions`) are enabled by default without being listed here. |
| `disabled`     | list of strings | `[]`    | Plugins to explicitly disable, including bundled ones.                                                                                       |
| `search_paths` | list of paths   | `[]`    | Additional directories to search for plugin binaries.                                                                                        |

Plugin-specific settings are configured under `[plugins.settings.<plugin-id>]`:

```toml
[plugins]
enabled = ["my-custom-plugin"]
disabled = ["bmux.permissions"]
search_paths = ["~/.local/share/bmux/plugins"]

[plugins.settings."bmux.windows"]
default_layout = "main-vertical"
```

---

## `[status_bar]`

Status bar display settings.

| Option              | Type    | Default | Description                             |
| ------------------- | ------- | ------- | --------------------------------------- |
| `left`              | string  | `""`    | Left side format string.                |
| `right`             | string  | `""`    | Right side format string.               |
| `update_interval`   | integer | `0`     | Status bar refresh interval in seconds. |
| `show_session_name` | bool    | `false` | Display the current session name.       |
| `show_window_list`  | bool    | `false` | Display the list of windows/panes.      |
| `show_mode`         | bool    | `false` | Display the current interaction mode.   |

```toml
[status_bar]
show_session_name = true
show_mode = true
update_interval = 1
```

---

## `[recording]`

Session recording settings. bmux can record terminal sessions for replay,
debugging, and playbook generation.

| Option           | Type    | Default          | Description                                                                                             |
| ---------------- | ------- | ---------------- | ------------------------------------------------------------------------------------------------------- |
| `dir`            | path    | _(XDG data dir)_ | Root directory for recording data. Relative paths resolve against the directory containing `bmux.toml`. |
| `enabled`        | bool    | `true`           | Enable the recording subsystem.                                                                         |
| `capture_input`  | bool    | `true`           | Capture pane input bytes (keystrokes).                                                                  |
| `capture_output` | bool    | `true`           | Capture pane output bytes (terminal output).                                                            |
| `capture_events` | bool    | `true`           | Capture lifecycle and server events.                                                                    |
| `segment_mb`     | integer | `64`             | Rotate recording segments at approximately this size in MB. Must be at least 1.                         |
| `retention_days` | integer | `30`             | Retention period for completed recordings in days. `0` disables automatic pruning.                      |

```toml
[recording]
enabled = true
capture_input = true
capture_output = true
segment_mb = 128
retention_days = 90
```

---

## Full Example

A complete `bmux.toml` with all sections:

```toml
[general]
default_mode = "normal"
mouse_support = true
default_shell = "/bin/zsh"
scrollback_limit = 50000
server_timeout = 5000

[appearance]
status_position = "BOTTOM"
pane_border_style = "ROUNDED"
show_pane_titles = true

[behavior]
pane_term = "bmux-256color"
restore_last_layout = true
confirm_quit_destroy = true
kitty_keyboard = true
stale_build_action = "error"
terminfo_auto_install = "ask"

[multi_client]
allow_independent_views = true

[keybindings]
prefix = "ctrl+a"
timeout_profile = "traditional"

[keybindings.runtime]
"d" = "detach"
"q" = "quit"

[plugins]
disabled = ["bmux.permissions"]

[status_bar]
show_session_name = true
show_mode = true

[recording]
enabled = true
retention_days = 90
```
