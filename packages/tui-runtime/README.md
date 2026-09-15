# bmux_tui_runtime

Bounded, domain-neutral scheduling and presentation runtime for terminal user interfaces built with `bmux_tui`.

The crate owns event admission, fair scheduling, commands, timers, redraw coalescing, render cadence, terminal input lifecycle, shutdown, and neutral runtime statistics. Application state and product behavior remain with the consumer.

## Inline protocol examples

Each example prints a red/blue checkerboard in the normal screen and exits,
leaving the image above the shell prompt. Run each in a terminal supporting its
protocol (or inside an updated BMUX, which translates to the host protocol):

```sh
cargo run -p bmux_tui_runtime --example image_runtime
cargo run -p bmux_tui_runtime --example image_sixel
cargo run -p bmux_tui_runtime --example image_iterm2
```

Kitty and iTerm2 request 16×8 cells. Sixel uses a 128×96-pixel raster and reserves
12 rows; use a terminal with at least 14 rows and cell height of at least 8 pixels.
These demos do not enter raw mode or the alternate screen. Image scrollback
retention depends on the terminal/multiplexer, not the shell's command history.

## Three-protocol alternate-screen demo

```sh
cargo run -p bmux_tui_runtime --example image_protocols
```

Shows three labeled checkerboards simultaneously, emitting each source protocol
independently. Press `q`, Escape, or Ctrl-C to quit. Resizing redraws the demo;
windows smaller than 60×14 show a resize message. Sixel uses a 64×48-pixel raster
(the other two request 12×6 cells), so sizes depend on font metrics. Use cells at
least 4×8 pixels. Inside BMUX the source protocols are translated to the host's
protocol; outside BMUX the terminal must support all three. The alternate screen
and terminal modes are restored on exit.

## Image-capable presentation

With the `images` feature, `ImageTerminalPresenter` connects protocol-neutral
`bmux_tui::image::ImageContribution` values to BMUX's host image compositor.
All three protocols and Crossterm are enabled by default. With defaults disabled,
select protocols through `image-kitty`, `image-sixel`, and `image-iterm2`.

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

Defaults enable `all-protocols` and `crossterm` independently. Use `--no-default-features`
(or `default-features = false` in a dependency declaration) for a minimal build,
then opt into individual features as needed.

- `crossterm`: managed Crossterm input and terminal event conversion.
- `images`: generic image-aware presentation without enabling a host protocol.
- `image-kitty`: Kitty graphics output.
- `image-sixel`: Sixel output.
- `all-protocols`: all three image protocols, without selecting an input/lifecycle backend.
- `image-iterm2`: iTerm2 inline-image output.
