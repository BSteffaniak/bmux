# bmux_clipboard

Platform-agnostic clipboard integration for bmux.

## Overview

Detects the correct system clipboard backend for the current OS and provides a
simple `copy_text` interface that spawns the backend process to write text to
the clipboard. Used by the clipboard plugin to implement copy operations from
scroll mode and other contexts.

## Features

- Auto-detection of platform clipboard commands (pbcopy, xclip, xsel, wl-copy, etc.)
- Fallback chains for Linux (Wayland and X11) and Windows (PowerShell clip.exe)
- Subprocess-based execution -- no C library bindings
- Graceful error reporting when no backend is available

## Core Types

- **`Clipboard`**: Handle that holds the detected backend command
- **`ClipboardError`**: `BackendUnavailable` (no command found) or `BackendFailed` (command exited non-zero)

## Usage

```rust
use bmux_clipboard::Clipboard;

let clipboard = Clipboard::new()?;
clipboard.copy_text("hello from bmux")?;

// Or use the convenience function:
bmux_clipboard::copy_text("hello from bmux")?;
```
