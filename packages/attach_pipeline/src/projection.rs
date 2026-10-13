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
    /// Legacy unsupported marker; new producers use a specific rejection.
    Unsupported,
    /// Specific, content-free rejection or transient deferral.
    Rejected(ProjectionFailure),
    Image(std::sync::Arc<bmux_tui::image::ImagePlacement>),
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

/// Content-free capture failure; never conflates unsupported output with admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionFailure {
    Budget {
        resource: &'static str,
        used: usize,
        limit: usize,
    },
    Unsupported {
        reason: &'static str,
    },
    Deferred {
        reason: &'static str,
    },
}
impl std::fmt::Display for ProjectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget {
                resource,
                used,
                limit,
            } => write!(
                f,
                "capture {resource} budget exceeded: used={used}, limit={limit}"
            ),
            Self::Unsupported { reason } => write!(f, "unsupported capture: {reason}"),
            Self::Deferred { reason } => write!(f, "capture deferred: {reason}"),
        }
    }
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
    image_resources: std::collections::BTreeSet<usize>,
    text_end: Option<(u16, u16, Style)>,
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

fn capture_bytes(operation: &PaintOperation) -> usize {
    if let PaintOperation::Image(image) = operation {
        return CAPTURE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(state) = slot.as_mut() else {
                return 0;
            };
            if state
                .image_resources
                .insert(std::sync::Arc::as_ptr(image) as usize)
            {
                operation_bytes(operation)
            } else {
                0
            }
        });
    }
    operation_bytes(operation)
}

/// Record bounded output; overflow explicitly invalidates the complete projection.
pub fn record(operation: PaintOperation) {
    record_with(capture_bytes(&operation), || operation);
}

/// Admit a borrowed operation before cloning it.
pub fn record_borrowed(operation: &PaintOperation) {
    record_with(capture_bytes(operation), || operation.clone());
}

fn operation_bytes(operation: &PaintOperation) -> usize {
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
        if state.operations.len() >= 262_144
            || bytes > (16 * 1024 * 1024_usize).saturating_sub(state.bytes)
        {
            let operation_limit = state.operations.len() >= 262_144;
            let used = if operation_limit {
                state.operations.len().saturating_add(1)
            } else {
                state.bytes.saturating_add(bytes)
            };
            state.operations.clear();
            state
                .operations
                .push(PaintOperation::Rejected(ProjectionFailure::Budget {
                    resource: if operation_limit {
                        "operations"
                    } else {
                        "payload bytes"
                    },
                    used,
                    limit: if operation_limit {
                        262_144
                    } else {
                        16 * 1024 * 1024
                    },
                }));
            state.failed = true;
            return;
        }
        state.bytes += bytes;
        let operation = make();
        let next_end = match &operation {
            PaintOperation::Text { x, y, text, style } => Some((
                x.saturating_add(bmux_plugin::render_text_width_u16(text)),
                *y,
                *style,
            )),
            _ => None,
        };
        if let Some(PaintOperation::Text { text, .. }) = state.operations.last_mut()
            && let PaintOperation::Text {
                x,
                y,
                text: next_text,
                style,
            } = &operation
            && state.text_end == Some((*x, *y, *style))
        {
            text.push_str(next_text);
            state.text_end = next_end;
            return;
        }
        state.text_end = next_end;
        if matches!(
            operation,
            PaintOperation::Unsupported | PaintOperation::Rejected(_)
        ) {
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
            PaintOperation::Unsupported | PaintOperation::Rejected(_) => {}
            PaintOperation::Image(image) => {
                cx.push_image(bmux_tui::image::ImageContribution::Present(
                    image.as_ref().clone(),
                ));
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
    fn dense_capture_coalesces_runs_and_shared_images_are_admitted_once() {
        use bmux_tui::image::{
            ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
        };
        let ((), operations) = capture_neutral(|| {
            for y in 0..300 {
                for x in 0..600 {
                    record(PaintOperation::Text {
                        x,
                        y,
                        text: "x".into(),
                        style: Style::default(),
                    });
                }
            }
        });
        assert_eq!(operations.len(), 300);
        assert!(
            operations
                .iter()
                .all(|op| matches!(op, PaintOperation::Text { text, .. } if text.len() == 600))
        );
        let image = std::sync::Arc::new(ImagePlacement {
            key: ImageKey::new("shared"),
            payload: ImagePayload::Pixels {
                bytes: vec![0; 8 * 1024 * 1024],
                width: 2048,
                height: 1024,
                format: ImagePixelFormat::Rgba8,
            },
            destination: Rect::new(0, 0, 10, 10),
            clip: Rect::new(0, 0, 10, 10),
            lifecycle: ImageLifecycle::Frame,
        });
        let ((), operations) = capture_neutral(|| {
            for _ in 0..8 {
                record_borrowed(&PaintOperation::Image(std::sync::Arc::clone(&image)));
            }
        });
        assert_eq!(operations.len(), 8);
        assert!(operations.iter().all(|op| matches!(op, PaintOperation::Image(captured) if std::sync::Arc::ptr_eq(&image, captured))));
    }

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
        assert_eq!(
            operations,
            vec![PaintOperation::Rejected(ProjectionFailure::Budget {
                resource: "payload bytes",
                used: 17 * 1024 * 1024,
                limit: 16 * 1024 * 1024,
            })]
        );
    }
}
