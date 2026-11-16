# bmux Prompted Actions Plugin

Bundled prompted-actions plugin for bmux.

## Overview

Provides config-driven prompted actions. A prompted action shows **one form**
with a field per configured prompt, substitutes the submitted values into a
command template, and dispatches the resulting bmux action.

This is useful for keybindings that need a small amount of runtime input, such
as recording export settings or command arguments.

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
title = "Cut Recording"
description = "Export the last N seconds of the recording as a GIF."
submit_label = "Export"
command = "plugin:bmux.recording:recording-cut --last-seconds {seconds} --export-fps {fps}"

[[plugins.settings."bmux.prompted_actions".actions.prompts]]
key = "seconds"
type = "text"
title = "Seconds"
placeholder = "last N seconds"
validation = "positive_integer"

[[plugins.settings."bmux.prompted_actions".actions.prompts]]
key = "fps"
type = "text"
title = "Frame rate"
message = "Frames per second of the exported GIF."
default = "24"
validation = "positive_integer"
```

Prompt `key` values are substituted into the action `command` by replacing
matching `{key}` placeholders.

### Action keys

| Key            | Meaning                                                               |
| -------------- | --------------------------------------------------------------------- |
| `title`        | Form title. Defaults to the only prompt's title, else the action name |
| `description`  | Text under the title explaining the action                            |
| `submit_label` | Submit button label (default `Run`)                                   |

### Prompt keys

| Key                      | Meaning                                                          |
| ------------------------ | ---------------------------------------------------------------- |
| `key`                    | Template placeholder name                                        |
| `type`                   | `text`, `confirm`, `single_select`, or `multi_toggle`            |
| `title`                  | Field label (`label` overrides it)                               |
| `message`                | Help text shown with the field                                   |
| `placeholder`            | Placeholder for text fields                                      |
| `required`               | Text fields only; defaults to `true`                             |
| `validation`             | `non_empty`, `positive_integer`, `integer`, `number`, or a regex |
| `default`                | String (text), bool (confirm), index (single), indices (multi)   |
| `options`                | Choices for `single_select` and `multi_toggle`                   |
| `min_selected`           | Minimum selections for `multi_toggle`                            |
| `yes_label` / `no_label` | Show a confirm as a labelled choice instead of a toggle          |

Submitted values are formatted for the template as: text and choices
verbatim, confirm as `true`/`false`, and multi-toggle values joined with `,`.

See [Prompts and forms](../../docs/prompts.md) for the form keys and layout
settings.
