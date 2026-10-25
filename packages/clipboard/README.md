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

## Native replacement (experimental)

`native-clipboard` opts into the owned native backends for text and PNG image
operations. Arboard remains the default/reference dependency until platform
interoperability and availability gates pass; enabling this feature does not
currently remove it from the dependency graph.

The Unix implementation uses pure-Rust `x11rb` and `wayland-client` transports,
without requiring `libxcb.so.1` or `libwayland-client.so.0`. Windows and macOS
use
native system APIs/frameworks. Clipboard payloads are bounded to 256 MiB with
explicit errors rather than silent truncation.

Tests must not overwrite a live user clipboard. The ignored `native_interop`
tests require both a disposable display and `BMUX_CLIPBOARD_ISOLATED_TEST=1`.
Isolated Xvfb tests cover text/image roundtrips, large-text interoperability
with
xclip, and acknowledged clipboard-manager shutdown handoff. Isolated headless
Sway tests cover text/image roundtrips and large-text interoperability with
wl-copy/wl-paste. The X11 normal-exit hook requests a bounded manager handoff;
persistence still requires a clipboard manager and is not promised after abort
or SIGKILL. Native Windows/macOS interoperability, independent image transfers,
and process-exit persistence need further acceptance evidence.
Existing source/diagnostic names containing `arboard` remain compatibility
labels while the backend is experimental.

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
