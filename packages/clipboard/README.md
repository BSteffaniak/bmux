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
- Failed native clipboard initialization or writes fall back through installed platform commands
- Linux command copying recovers a missing `WAYLAND_DISPLAY` when `XDG_RUNTIME_DIR`
  contains exactly one same-owner `wayland-<number>` socket. This recovery is local
  to the subprocess, ignores symlinks and unrelated sockets, and is disabled over
  SSH or when a display is explicitly configured. Multiple desktops are not guessed.
- Graceful error reporting including each attempted backend when copying fails

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
