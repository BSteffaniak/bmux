//! Backend-neutral output captured from the same lowering plans as terminal delivery.
//! Capture is synchronous and scoped; it never parses encoded terminal bytes.
use std::cell::RefCell;

use bmux_plugin::{RenderColor, RenderNamedColor, RenderStyle};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span};

/// One already clipped operation from terminal output planning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaintOperation {
    /// Unsupported imperative output must disconnect rather than claim a full view.
    Unsupported,
    Image(bmux_tui::image::ImagePlacement),
    Text {
        x: u16,
        y: u16,
        text: String,
        style: Style,
    },
    Fill {
        rect: Rect,
        style: Style,
    },
}

thread_local! {
    static ENCODE_TERMINAL: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Whether synchronous composition should lower terminal control bytes.
#[must_use]
pub fn encoding_terminal() -> bool {
    ENCODE_TERMINAL.with(std::cell::Cell::get)
}

/// Run the same lowering plans without invoking terminal encoders.
///
/// # Panics
/// Panics if called inside another synchronous capture.
pub fn capture_neutral<T>(run: impl FnOnce() -> T) -> (T, Vec<PaintOperation>) {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            ENCODE_TERMINAL.with(|flag| flag.set(self.0));
        }
    }
    let reset = Reset(ENCODE_TERMINAL.with(|flag| flag.replace(false)));
    let result = capture(run);
    drop(reset);
    result
}

#[derive(Default)]
struct CaptureState {
    operations: Vec<PaintOperation>,
    bytes: usize,
    failed: bool,
}
thread_local! {
    static CAPTURE: RefCell<Option<CaptureState>> = const { RefCell::new(None) };
}
struct CaptureReset;
impl Drop for CaptureReset {
    fn drop(&mut self) {
        CAPTURE.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

/// Capture neutral operations while running synchronous terminal planning.
/// No capture may span an await point.
///
/// # Panics
/// Panics on nested capture without replacing the existing capture.
pub fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<PaintOperation>) {
    CAPTURE.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "nested output capture");
        *slot = Some(CaptureState::default());
    });
    let reset = CaptureReset;
    let result = run();
    let operations = CAPTURE.with(|slot| {
        slot.borrow_mut()
            .take()
            .map_or_else(Vec::new, |state| state.operations)
    });
    drop(reset);
    (result, operations)
}

/// Record bounded output; overflow explicitly invalidates the complete projection.
pub fn record(operation: PaintOperation) {
    record_with(operation_bytes(&operation), || operation);
}

/// Admit a borrowed operation before cloning it.
pub fn record_borrowed(operation: &PaintOperation) {
    record_with(operation_bytes(operation), || operation.clone());
}

const fn operation_bytes(operation: &PaintOperation) -> usize {
    match operation {
        PaintOperation::Text { text, .. } => text.len(),
        PaintOperation::Image(image) => match &image.payload {
            bmux_tui::image::ImagePayload::Png { bytes, .. }
            | bmux_tui::image::ImagePayload::Pixels { bytes, .. } => bytes.len(),
        },
        _ => 0,
    }
}

/// Admit a resource before cloning or allocating its captured representation.
pub fn record_with(bytes: usize, make: impl FnOnce() -> PaintOperation) {
    CAPTURE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        if state.failed {
            return;
        }
        if state.operations.len() >= 131_072
            || bytes > (16 * 1024 * 1024_usize).saturating_sub(state.bytes)
        {
            state.operations.clear();
            state.operations.push(PaintOperation::Unsupported);
            state.failed = true;
            return;
        }
        state.bytes += bytes;
        let operation = make();
        if matches!(operation, PaintOperation::Unsupported) {
            state.operations.clear();
            state.failed = true;
        }
        state.operations.push(operation);
    });
}

/// Owned operations staged so far in this synchronous composition.
#[must_use]
pub fn staged_operations() -> Vec<PaintOperation> {
    CAPTURE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(Vec::new, |state| state.operations.clone())
    })
}

/// Whether synchronous output planning has an observer.
#[must_use]
pub fn capturing() -> bool {
    CAPTURE.with(|slot| slot.borrow().is_some())
}

/// Replay output operations through the authoritative clipped paint context.
pub fn paint(cx: &mut PaintCx<'_, '_>, operations: &[PaintOperation]) {
    for operation in operations {
        match operation {
            PaintOperation::Unsupported => {}
            PaintOperation::Image(image) => {
                cx.push_image(bmux_tui::image::ImageContribution::Present(image.clone()));
            }
            PaintOperation::Text { x, y, text, style } => {
                let width = bmux_plugin::render_text_width_u16(text);
                cx.write_line(
                    LocalRect::new(i32::from(*x), i64::from(*y), width, 1),
                    &Line::from_spans(vec![Span::styled(text.clone(), *style)]),
                );
            }
            PaintOperation::Fill { rect, style } => {
                cx.fill(LocalRect::terminal(*rect), " ", *style);
            }
        }
    }
}

/// Preserve all declarative terminal style attributes without terminal encoding.
#[must_use]
pub fn style(value: RenderStyle) -> Style {
    let mut result = Style::new();
    if let Some(color) = value.fg {
        result = result.fg(render_color(color));
    }
    if let Some(color) = value.bg {
        result = result.bg(render_color(color));
    }
    for (enabled, flag) in [
        (value.bold, Modifier::BOLD),
        (value.dim, Modifier::DIM),
        (value.italic, Modifier::ITALIC),
        (value.underline, Modifier::UNDERLINE),
        (value.reverse, Modifier::REVERSED),
        (value.blink, Modifier::SLOW_BLINK),
        (value.strikethrough, Modifier::CROSSED_OUT),
    ] {
        if enabled {
            result = result.add_modifier(flag);
        }
    }
    result
}

const fn render_color(value: RenderColor) -> Color {
    match value {
        RenderColor::Default => Color::Default,
        RenderColor::Indexed(index) => Color::Indexed(index),
        RenderColor::Rgb { r, g, b } => Color::Rgb(r, g, b),
        RenderColor::Named(named) => match named {
            RenderNamedColor::Black => Color::Black,
            RenderNamedColor::Red => Color::Red,
            RenderNamedColor::Green => Color::Green,
            RenderNamedColor::Yellow => Color::Yellow,
            RenderNamedColor::Blue => Color::Blue,
            RenderNamedColor::Magenta => Color::Magenta,
            RenderNamedColor::Cyan => Color::Cyan,
            RenderNamedColor::White => Color::White,
            RenderNamedColor::BrightBlack => Color::BrightBlack,
            RenderNamedColor::BrightRed => Color::BrightRed,
            RenderNamedColor::BrightGreen => Color::BrightGreen,
            RenderNamedColor::BrightYellow => Color::BrightYellow,
            RenderNamedColor::BrightBlue => Color::BrightBlue,
            RenderNamedColor::BrightMagenta => Color::BrightMagenta,
            RenderNamedColor::BrightCyan => Color::BrightCyan,
            RenderNamedColor::BrightWhite => Color::BrightWhite,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_capture_preserves_outer_state_and_byte_admission_precedes_allocation() {
        let ((), operations) = capture(|| {
            assert!(std::panic::catch_unwind(|| capture(|| {})).is_err());
            record(PaintOperation::Fill {
                rect: Rect::new(0, 0, 1, 1),
                style: Style::new(),
            });
        });
        assert_eq!(operations.len(), 1);
        let ((), operations) = capture(|| {
            record_with(17 * 1024 * 1024, || {
                panic!("must not allocate rejected resource")
            });
        });
        assert_eq!(operations, vec![PaintOperation::Unsupported]);
    }
}
