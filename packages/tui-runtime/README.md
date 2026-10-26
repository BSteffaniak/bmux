# bmux_tui_runtime

Bounded, domain-neutral scheduling and presentation runtime for terminal user
interfaces built with `bmux_tui`.

The crate owns event admission, fair scheduling, commands, timers, redraw
coalescing, render cadence, terminal input lifecycle, shutdown, and neutral
runtime statistics. Application state and product behavior remain with the
consumer.

## Committed interaction and pointer presentation

Use `TerminalPresenter::with_commit` to copy the successfully flushed hit scene
and active focus scope into the application's `InteractionRouter`. Route later
input through that committed scene, not geometry reconstructed during dispatch.
A failed presentation must leave the previous interaction scene authoritative.
The router's `pointer_intent()` derives presentation from the same scene and
existing press/capture owner; it does not route another event or establish a
second focus authority. Re-evaluate after both input and successful scene
commits
so a stationary pointer observes disabled controls, modal changes, and removal.

Intent resolution alone does not emit OSC 22 or establish terminal capability.
Do not write pointer escape sequences from a control, render callback, or commit
observer: output belongs to the standard terminal/presenter backend. Host
support
for a shape, and especially for `PointerIntent::Hidden`, must not be inferred
from the presence of a neutral intent API. Pointer invisibility is independent
of text-caret visibility.

Applications own terminal lifecycle: stop and join managed input before
releasing
terminal ownership, attempt cleanup on application errors as well as normal
exit,
and propagate restoration errors. `Presenter::restore` restores pointer
ownership
through the same output owner. `Runtime::run` invokes it on exit and update or
presentation failure; `RuntimeError` retains any additional cleanup error. When
driving a presenter directly, call `restore().await` before leaving the terminal
guard, and still attempt guard restoration if pointer restoration fails.

Wrap `TerminalPresenter` in `PointerContextPresenter` to supply the
application's
existing modal scope and capture owner. This adapter forwards input observation,
presentation, and restoration; it does not own another input router. Call
`observe_terminal_event` on the presenter before dispatch when driving it
without
`Runtime`; the runtime does this automatically.

### Interactive pointer controls

```sh
cargo run -p bmux_tui_runtime --example pointer_controls --no-default-features --features crossterm
```

The example uses `TerminalPresenter`, `PointerContextPresenter`, and one
application-owned `InteractionRouter`. It includes a click counter, editable
text,
a permanently disabled button, a modal, and a draggable handle whose pointer
capture survives leaving its bounds. Tab/Shift-Tab traverse focus; F2 toggles
the
modal; F3 toggles an explicit crosshair override on the action; F4 toggles that
action's disabled state. Escape closes the modal and Ctrl-C exits. Keep the
pointer stationary while toggling F3/F4 to observe scene-driven changes. The
example uses bounded synchronous polling, not a competing asynchronous reader,
and attempts pointer and outer terminal restoration on normal exit and errors.

Host capability detection remains conservative: unsupported hosts still display
and operate the controls but receive no pointer-shape output. The example does
not force support or demonstrate pointer invisibility. OSC 22 setter-only hosts
restore the default shape; stack-capable hosts release the owned stack lease.
It does not claim restoration of a previously unknown setter-only host shape.

The example's in-file tests exercise real standard-presenter commits, control
actions and editing, capture release, modal exclusion, stationary-pointer
overrides, disabled-state changes, and restoration error reporting. Run them
with `cargo test -p bmux_tui_runtime --example pointer_controls
--no-default-features --features crossterm`.

## Inline protocol examples

Each example prints a red/blue checkerboard in the normal screen and exits,
leaving the image above the shell prompt. Run each in a terminal supporting its
protocol (or inside an updated BMUX, which translates to the host protocol):

```sh
cargo run -p bmux_tui_runtime --example image_runtime -- --protocol kitty
cargo run -p bmux_tui_runtime --example image_runtime -- --protocol sixel
cargo run -p bmux_tui_runtime --example image_runtime -- --protocol iterm2
cargo run -p bmux_tui_runtime --example image_sixel
cargo run -p bmux_tui_runtime --example image_iterm2
```

With exactly one protocol feature enabled, `image_runtime` selects it
automatically:

```sh
cargo run -p bmux_tui_runtime --example image_runtime --no-default-features --features image-sixel
```

Multiple enabled protocols require `--protocol`; disabled or unknown protocols
produce an error before any terminal output. Selection does not probe the
terminal
or silently fall back to another protocol.

Kitty and iTerm2 request 16×8 cells. Sixel uses a 128×96-pixel raster and
reserves
12 rows; use a terminal with at least 14 rows and cell height of at least 8
pixels.
These demos do not enter raw mode or the alternate screen. Image scrollback
retention depends on the terminal/multiplexer, not the shell's command history.

## Three-protocol alternate-screen demo

```sh
cargo run -p bmux_tui_runtime --example image_protocols
```

Shows three labeled checkerboards simultaneously, emitting each source protocol
independently. Press `q`, Escape, or Ctrl-C to quit. Resizing redraws the demo;
windows smaller than 60×14 show a resize message. Sixel uses a 64×48-pixel
raster
(the other two request 12×6 cells), so sizes depend on font metrics. Use cells
at
least 4×8 pixels. Inside BMUX the source protocols are translated to the host's
protocol; outside BMUX the terminal must support all three. The alternate screen
and terminal modes are restored on exit.

## Image-capable presentation

With the `images` feature, `ImageTerminalPresenter` connects protocol-neutral
`bmux_tui::image::ImageContribution` values to BMUX's host image compositor.
All three protocols and Crossterm are enabled by default. With defaults
disabled,
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

Defaults enable `all-protocols` and `crossterm` independently. Use
`--no-default-features`
(or `default-features = false` in a dependency declaration) for a minimal build,
then opt into individual features as needed.

- `crossterm`: managed Crossterm input and terminal event conversion.
- `images`: generic image-aware presentation without enabling a host protocol.
- `image-kitty`: Kitty graphics output.
- `image-sixel`: Sixel output.
- `all-protocols`: all three image protocols, without selecting an input/lifecycle backend.
- `image-iterm2`: iTerm2 inline-image output.
