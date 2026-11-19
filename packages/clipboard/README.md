# bmux_clipboard

BMUX's clipboard helpers use its own native backend: AppKit on macOS, Win32 on
Windows, and Wayland or X11 on supported Unix desktops.

## Text and images

- `copy_text` and `read_text` try the native backend first and retain platform command fallbacks for backend failures. `native_text::{read, write}` never starts a command; `read` distinguishes an absent text format from an empty string.
- `Clipboard::new()` selects a clipboard command for callers using the command-only handle.
- `copy_png_image` publishes an image; payload reads can return PNG bytes, including images recovered from a clipboard file reference.
- On Linux, command copying can recover a missing `WAYLAND_DISPLAY` when `XDG_RUNTIME_DIR` contains exactly one same-owner `wayland-<number>` socket. Recovery is local to the subprocess and does not follow symlinks or guess between desktops.
- X11 persistence after normal writer exit depends on a clipboard manager. Handoff is bounded and is not guaranteed after abort or SIGKILL.

## Testing

Never run clipboard interop tests against a personal clipboard. Use a disposable display for the ignored native roundtrip tests. The [macOS interop probe](../../scripts/clipboard/macos/README.md) uses a unique named pasteboard and does not exercise general-board permissions or GUI gestures. See the [Windows interop instructions](../../scripts/clipboard/windows/README.md) for that platform's isolation requirements.
