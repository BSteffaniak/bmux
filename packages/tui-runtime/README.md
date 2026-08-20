# bmux_tui_runtime

Bounded, domain-neutral scheduling and presentation runtime for terminal user interfaces built with `bmux_tui`.

The crate owns event admission, fair scheduling, commands, timers, redraw coalescing, render cadence, terminal input lifecycle, shutdown, and neutral runtime statistics. Application state and product behavior remain with the consumer.

## Image-capable presentation

With the `images` feature, `ImageTerminalPresenter` connects protocol-neutral
`bmux_tui::image::ImageContribution` values to BMUX's host image compositor.
Protocol implementations remain opt-in through `image-kitty`, `image-sixel`,
and `image-iterm2`.

The presenter commits cell output, the reconciled image scene, and interaction
metadata through one synchronized terminal update and flush. A failed cell or
image write does not advance retained frame, image, hit, focus, or selection
state. Stable image keys retain protocol resources across placement-only
updates; changed payloads and removed or fully clipped images delete stale host
resources.

Use `ImageTerminalPresenter::detect` for environment-only capability detection,
which performs no terminal I/O. If active capability queries are required, call
`bmux_image::host_caps::detect_with_queries` after entering raw mode but before
starting `ManagedTerminalInput`, then pass the result to
`ImageTerminalPresenter::new`.

Applications must call `cleanup_images` before returning terminal ownership,
including graceful exit, suspension, or recovery from an application error.
`reset_presentation` performs cleanup and invalidates retained output. Runtime
lifecycle integration should keep the presenter owned until this cleanup has
completed.

See [`../../docs/tui-runtime.md`](../../docs/tui-runtime.md) for the full architecture contract.

## Cargo features

- `crossterm`: managed Crossterm input and terminal event conversion.
- `images`: generic image-aware presentation without enabling a host protocol.
- `image-kitty`: Kitty graphics output.
- `image-sixel`: Sixel output.
- `image-iterm2`: iTerm2 inline-image output.

