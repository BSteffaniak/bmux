# bmux_tui

Native terminal UI primitives for BMUX.

This crate is intentionally domain-agnostic. It provides reusable geometry,
layout, styled text, and render-buffer foundations for terminal interfaces.
BMUX sessions, windows, panes, clients, contexts, permissions, and product UI
behavior live outside this crate.

See [`../../docs/tui-framework.md`](../../docs/tui-framework.md) for the broader architecture and roadmap.

## Pointer intent and committed geometry

`pointer::PointerIntent` describes the mouse pointer, not the text caret.
`Auto` derives a shape from `HitRole`; `Shape(PointerShape)` explicitly selects
one of the shared terminal protocol shapes. `Hidden` expresses host-dependent
pointer invisibility; it is not an OSC 22 shape or a caret-visibility operation.

Set `HitRegion::pointer_intent` for an explicit region override. Resolve against
the last successfully committed scene with `InteractionRouter::pointer_intent`,
or use `pointer::resolve_pointer_intent` with the existing capture identity and
active scope. Do not create a second input router for pointer presentation.

Automatic roles select text for text inputs, pointer for actions/list items,
horizontal resize for resize handles, and grabbing for captured drag handles.
Explicit intent survives capture. Valid capture retains its owner's intent
outside its bounds; removed, disabled, invisible, empty, non-pointer, or
out-of-scope owners no longer capture presentation. Disabled and out-of-scope
regions occlude lower regions without supplying their own override. Re-resolve
after scene commits even when the pointer is stationary.

```rust
use bmux_tui::{
    geometry::{Point, Rect},
    hit::{HitMap, HitRegion, HitRole},
    pointer::{PointerIntent, PointerShape, resolve_pointer_intent},
};

let scene = HitMap::new().with_region(
    HitRegion::new("input", Rect::new(0, 0, 20, 1)).role(HitRole::TextInput),
);
assert_eq!(
    resolve_pointer_intent(&scene, Some(Point::new(1, 0)), None, None),
    PointerIntent::Shape(PointerShape::Text),
);
```

This example resolves metadata only; it does not emit terminal output. A backend
must explicitly support the resolved intent. Keep output and lifecycle handling
in the standard terminal/presenter path rather than writing OSC sequences from
controls or render callbacks. The runtime's
[`pointer_controls` example](../tui-runtime/examples/pointer_controls.rs) uses
that path for buttons, editable text, disabled controls, a modal, capture, and
explicit shape overrides; see its [run instructions](../tui-runtime/README.md#interactive-pointer-controls).

## Streaming subprocess output

`ansi::AnsiLog` incrementally decodes byte chunks into a bounded styled log
tail.
It retains split UTF-8/SGR sequences, supports carriage-return overwrite and
`CSI K`, strips unsupported controls, and limits both lines and line length.
It is a log decoder, not a full terminal emulator. Use one decoder per stream;
applications retain complete transcripts separately if required.

`inline_output::InlineOutput` paints that tail without raw mode or an alternate
screen. It supports differential redraws, caller-driven rate limiting, explicit
clearing, and permanent styled output via `commit`:

```rust,no_run
use std::{io, time::{Duration, Instant}};
use bmux_tui::{ansi::AnsiLog, geometry::Size, inline_output::InlineOutput, text::Line};

# fn main() -> io::Result<()> {
let mut log = AnsiLog::new(200, 4096);
let mut output = InlineOutput::new(io::stderr(), Duration::from_millis(33));
// Obtain fresh terminal dimensions through your terminal backend on every update.
let size = Size::new(80, 24);
log.push(b"\x1b[32mCompiling example\x1b[0m\n");
let tail = log.tail(usize::from(size.width - 1), 10);
output.draw(&tail, size, Instant::now(), true)?;
// On success replace transient chatter with a marker. On failure, commit the
// separately captured full transcript, decoded with the same safe text policy.
output.commit(&[Line::raw("validated example")], size)?;
output.clear(size)?;
# Ok(())
# }
```

The caller owns TTY detection and must use ordinary streaming for redirected
output. All concurrent output must go through the presenter. Throttled draws
return `false`: keep the latest model and call again at the next deadline (or
force the final draw). No worker or timer is spawned by the presenter.

Resizes invalidate cursor anchors. The presenter intentionally retains the old
region on resize instead of moving upwards and potentially erasing permanent
history. A new bounded region is then established. Call `clear` with fresh
terminal dimensions to finish; dropping it does not move the cursor or change
terminal modes. Interrupted output therefore remains visible and the terminal
stays in its original input mode. Writer errors poison the anchor and are
returned rather than risking further destructive redraws.
