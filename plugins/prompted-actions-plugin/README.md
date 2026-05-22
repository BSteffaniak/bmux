# bmux Prompted Actions Plugin

Bundled prompted-actions plugin for bmux.

## Overview

Provides config-driven prompted action sequences. A prompted action displays one
or more prompt overlays, collects values from the user, substitutes those values
into a command template, and dispatches the resulting bmux action.

This is useful for keybindings that need a small amount of runtime input, such as
recording export settings or command arguments.

## Command

- **`prompted-action run ACTION`** — execute a configured prompted action by name

From a keybinding, use the plugin command form:

```toml
[keybindings.global]
"ctrl+alt+r" = "plugin:bmux.prompted_actions:run recording-cut"
```

## Configuration

Actions are configured under `plugins.settings."bmux.prompted_actions"`:

```toml
[plugins.settings."bmux.prompted_actions"]

[[plugins.settings."bmux.prompted_actions".actions]]
name = "recording-cut"
command = "plugin:bmux.plugin_cli:recording-cut --last-seconds {seconds} --export-fps {fps}"

[[plugins.settings."bmux.prompted_actions".actions.prompts]]
key = "seconds"
type = "text"
title = "Cut Recording"
placeholder = "last N seconds"
validation = "positive_integer"

[[plugins.settings."bmux.prompted_actions".actions.prompts]]
key = "fps"
type = "text"
title = "GIF Frame Rate"
placeholder = "24"
default = "24"
validation = "positive_integer"
```

Prompt `key` values are substituted into the action `command` by replacing
matching `{key}` placeholders.

## Prompt types

Supported prompt types:

- `text`
- `confirm`
- `single_select`
- `multi_toggle`

Supported text validation rules:

- `non_empty`
- `positive_integer`
- `integer`
- `number`
- custom regex string
