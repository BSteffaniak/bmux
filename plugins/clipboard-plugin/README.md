# bmux_clipboard_plugin

Bundled clipboard plugin for bmux.

## Overview

Provides the `clipboard-write/v1` service that other plugins and the host
runtime use to write text to the system clipboard. Delegates to the
`bmux_clipboard` crate for platform-specific backend detection. This plugin is
statically linked into the bmux binary when the `bundled-plugin-clipboard`
feature is enabled.

## Services

- **`clipboard-write/v1`**
  - `copy_text` -- writes the provided text to the system clipboard via the detected platform backend
