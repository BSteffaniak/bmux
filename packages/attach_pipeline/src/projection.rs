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
    Grid(std::sync::Arc<bmux_tui::presentation::PreparedGridFragment>),
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
        PaintOperation::Grid(fragment) => fragment.buffer().cells().iter().fold(
            fragment
                .images()
                .iter()
                .fold(0_usize, |bytes, image| {
                    bytes
                        .saturating_add(operation_bytes(&PaintOperation::Image(
                            std::sync::Arc::clone(image),
                        )))
                        .saturating_add(image.key.as_str().len())
                })
                .saturating_add(
                    fragment
                        .buffer()
                        .cells()
                        .len()
                        .saturating_mul(std::mem::size_of::<bmux_tui::buffer::Cell>())
                        .saturating_add(fragment.id().len())
                        .saturating_add(
                            fragment
                                .visible_clips()
                                .len()
                                .saturating_mul(std::mem::size_of::<Rect>()),
                        ),
                ),
            |bytes, cell| bytes.saturating_add(cell.symbol.len()),
        ),
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

/// Replay uniform output, rejecting local grids before painting anything.
/// # Errors
/// Returns unsupported for local-grid output; use `paint_with_grid_fragments`.
pub fn paint(
    cx: &mut PaintCx<'_, '_>,
    operations: &[PaintOperation],
) -> Result<(), ProjectionFailure> {
    if operations
        .iter()
        .any(|operation| matches!(operation, PaintOperation::Grid(_)))
    {
        return Err(ProjectionFailure::Unsupported {
            reason: "backend requires uniform grid",
        });
    }
    paint_with_grid_fragments(cx, operations).map(|_| ())
}

/// Replay root operations and return retained grids for bounded frame admission.
///
/// The caller must install the returned grids with `set_grid_fragments` before
/// output; silently discarding them is not a supported backend fallback.
/// # Errors
/// Returns captured rejection before painting anything.
pub fn paint_with_grid_fragments(
    cx: &mut PaintCx<'_, '_>,
    operations: &[PaintOperation],
) -> Result<Vec<bmux_tui::presentation::PreparedGridFragment>, ProjectionFailure> {
    for operation in operations {
        match operation {
            PaintOperation::Rejected(reason) => return Err(reason.clone()),
            PaintOperation::Unsupported => {
                return Err(ProjectionFailure::Unsupported {
                    reason: "legacy imperative capture",
                });
            }
            _ => {}
        }
    }
    let mut fragments = Vec::new();
    for operation in operations {
        match operation {
            PaintOperation::Grid(fragment) => fragments.push(fragment.as_ref().clone()),
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
    Ok(fragments)
}

/// Lower only original retained paint, never flattened root cells.
///
/// Each span is sparse and retains its optional background. Clips are applied again against
/// the fragment's final winning clips by the backend.
///
/// # Errors
/// Rejects unsupported glyph geometry and exhausted output budgets before output.
// This is one ordered lowering pass; keeping the RenderOp match next to the
// shared budgeted row emitter makes sparse paint semantics auditable.
#[allow(clippy::too_many_lines)]
pub fn prepare_content_overlays(
    plan: &crate::compositor::ContentPaintPlan,
) -> Result<Vec<bmux_tui::presentation::PreparedGridOverlay>, ProjectionFailure> {
    use bmux_plugin::RenderOp;
    use bmux_tui::{buffer::Buffer, presentation::PreparedGridOverlay};
    if plan.overlays.len() > 4096 {
        return Err(ProjectionFailure::Budget {
            resource: "overlay operations",
            used: plan.overlays.len(),
            limit: 4096,
        });
    }
    let mut input_work = 0_usize;
    for overlay in &plan.overlays {
        let count = match &overlay.operation {
            RenderOp::StyledText { spans, .. } => spans.len(),
            RenderOp::CellGrid { rows, .. } => {
                if rows.len() > 1_000_000 {
                    return Err(ProjectionFailure::Budget {
                        resource: "overlay input rows",
                        used: rows.len(),
                        limit: 1_000_000,
                    });
                }
                rows.iter()
                    .fold(rows.len(), |n, row| n.saturating_add(row.len()))
            }
            _ => 1,
        };
        input_work = input_work.saturating_add(count);
        if input_work > 1_000_000 {
            return Err(ProjectionFailure::Budget {
                resource: "overlay input work",
                used: input_work,
                limit: 1_000_000,
            });
        }
    }
    let mut output = Vec::new();
    let mut cells = 0_usize;
    let mut bytes = 0_usize;
    let mut work = 0_usize;
    for overlay in &plan.overlays {
        let clip = Rect::new(
            overlay.clip.x,
            overlay.clip.y,
            overlay.clip.w,
            overlay.clip.h,
        );
        let mut row =
            |x: u16, y: u16, text: &str, value: RenderStyle| -> Result<(), ProjectionFailure> {
                if text.len() > 16 * 1024 * 1024 {
                    return Err(ProjectionFailure::Budget {
                        resource: "overlay text bytes",
                        used: text.len(),
                        limit: 16 * 1024 * 1024,
                    });
                }
                bytes = bytes.saturating_add(text.len());
                if bytes > 16 * 1024 * 1024 {
                    return Err(ProjectionFailure::Budget {
                        resource: "overlay text bytes",
                        used: bytes,
                        limit: 16 * 1024 * 1024,
                    });
                }
                if text.chars().any(char::is_control) {
                    return Err(ProjectionFailure::Unsupported {
                        reason: "control character in original overlay paint",
                    });
                }
                work = work.saturating_add(1);
                let width = bmux_plugin::render_text_width_u16(text);
                let area = Rect::new(x, y, width.min(u16::MAX - x), 1);
                let effective = area.intersection(clip);
                if work > 1_000_000 {
                    return Err(ProjectionFailure::Budget {
                        resource: "overlay work",
                        used: work,
                        limit: 1_000_000,
                    });
                }
                if effective.is_empty() {
                    return Ok(());
                }
                cells = cells.saturating_add(usize::from(area.width));
                if cells > 1_000_000 || bytes > 16 * 1024 * 1024 || output.len() >= 4096 {
                    return Err(ProjectionFailure::Budget {
                        resource: "overlay cells/bytes/spans",
                        used: cells.max(bytes).max(output.len()),
                        limit: 1_000_000,
                    });
                }
                let mut buffer = Buffer::empty(area);
                buffer.write_line(
                    area,
                    &Line::from_spans(vec![Span::styled(text, style(value))]),
                );
                let prepared = PreparedGridOverlay::new(
                    std::sync::Arc::new(buffer),
                    effective,
                    value.bg.is_some() || value.reverse,
                )
                .ok_or(ProjectionFailure::Unsupported {
                    reason: "overlay geometry",
                })?;
                output.push(prepared);
                Ok(())
            };
        match &overlay.operation {
            RenderOp::TextRun { x, y, text, style } => row(*x, *y, text, *style)?,
            RenderOp::StyledText { x, y, spans } => {
                let mut x = *x;
                for span in spans {
                    row(x, *y, &span.text, span.style)?;
                    x = x.saturating_add(bmux_plugin::render_text_width_u16(&span.text));
                }
            }
            RenderOp::ClearRect { rect, style }
            | RenderOp::FillRect {
                rect,
                ch: ' ',
                style,
            } => {
                let text = " ".repeat(usize::from(rect.w));
                for y in rect.y..rect.bottom() {
                    row(rect.x, y, &text, *style)?;
                }
            }
            RenderOp::EraseRowSegment { x, y, width, style } => {
                row(*x, *y, &" ".repeat(usize::from(*width)), *style)?;
            }
            RenderOp::FillRect { rect, ch, style } => {
                if bmux_plugin::render_text_width_u16(&ch.to_string()) != 1 {
                    return Err(ProjectionFailure::Unsupported {
                        reason: "non-single-cell overlay fill",
                    });
                }
                let text = ch.to_string().repeat(usize::from(rect.w));
                for y in rect.y..rect.bottom() {
                    row(rect.x, y, &text, *style)?;
                }
            }
            RenderOp::Border {
                rect,
                glyphs,
                style,
            } => {
                let glyph = [
                    glyphs.horizontal,
                    glyphs.vertical,
                    glyphs.top_left,
                    glyphs.top_right,
                    glyphs.bottom_left,
                    glyphs.bottom_right,
                ];
                if glyph
                    .iter()
                    .any(|ch| bmux_plugin::render_text_width_u16(&ch.to_string()) != 1)
                {
                    return Err(ProjectionFailure::Unsupported {
                        reason: "non-single-cell overlay border",
                    });
                }
                if rect.w == 0 || rect.h == 0 {
                    continue;
                }
                for y in rect.y..rect.bottom() {
                    if rect.h == 1 {
                        row(
                            rect.x,
                            y,
                            &glyphs.horizontal.to_string().repeat(usize::from(rect.w)),
                            *style,
                        )?;
                    } else if rect.w == 1 {
                        row(rect.x, y, &glyphs.vertical.to_string(), *style)?;
                    } else if y == rect.y || y == rect.bottom() - 1 {
                        let (left, right) = if y == rect.y {
                            (glyphs.top_left, glyphs.top_right)
                        } else {
                            (glyphs.bottom_left, glyphs.bottom_right)
                        };
                        row(
                            rect.x,
                            y,
                            &format!(
                                "{left}{}{right}",
                                glyphs
                                    .horizontal
                                    .to_string()
                                    .repeat(usize::from(rect.w - 2))
                            ),
                            *style,
                        )?;
                    } else {
                        row(rect.x, y, &glyphs.vertical.to_string(), *style)?;
                        row(rect.right() - 1, y, &glyphs.vertical.to_string(), *style)?;
                    }
                }
            }
            RenderOp::CellGrid { x, y, rows } => {
                for (dy, cells) in rows.iter().enumerate() {
                    let Some(y) = u16::try_from(dy).ok().and_then(|dy| y.checked_add(dy)) else {
                        break;
                    };
                    for (dx, cell) in cells.iter().enumerate() {
                        let Some(x) = u16::try_from(dx).ok().and_then(|dx| x.checked_add(dx))
                        else {
                            break;
                        };
                        if let Some(ch) = cell.ch {
                            row(x, y, &ch.to_string(), cell.style)?;
                        }
                    }
                }
            }
        }
    }
    Ok(output)
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
    fn original_overlay_spans_are_sparse_clipped_and_reverse_is_explicit() {
        use crate::{
            compositor::{ContentPaintOverlay, ContentPaintPlan},
            render::DamageRect,
        };
        let make = |operation| ContentPaintOverlay {
            surface_id: uuid::Uuid::nil(),
            owner: None,
            revision: Some(7),
            operation_index: 0,
            operation,
            clip: DamageRect::new(2, 0, 2, 1),
        };
        let plan = ContentPaintPlan {
            clips: vec![DamageRect::new(0, 0, 6, 2)],
            overlays: vec![
                make(bmux_plugin::RenderOp::text_run(
                    1,
                    0,
                    "界x",
                    RenderStyle::new(),
                )),
                make(bmux_plugin::RenderOp::text_run(
                    2,
                    0,
                    "r",
                    RenderStyle {
                        reverse: true,
                        ..RenderStyle::new()
                    },
                )),
            ],
        };
        let spans = prepare_content_overlays(&plan).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].buffer().area(), Rect::new(1, 0, 3, 1));
        assert_eq!(spans[0].clip(), Rect::new(2, 0, 2, 1));
        assert_eq!(
            spans[0]
                .buffer()
                .get(bmux_tui::geometry::Point::new(1, 0))
                .unwrap()
                .symbol,
            "界"
        );
        assert!(!spans[0].paint_background());
        assert!(spans[1].paint_background());
        assert!(
            spans[1].buffer().cells()[0]
                .style
                .modifiers
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn sparse_cell_grid_does_not_paint_holes_and_budgets_reject() {
        use crate::{
            compositor::{ContentPaintOverlay, ContentPaintPlan},
            render::DamageRect,
        };
        let mut plan = ContentPaintPlan {
            clips: vec![],
            overlays: vec![ContentPaintOverlay {
                surface_id: uuid::Uuid::nil(),
                owner: None,
                revision: None,
                operation_index: 0,
                operation: bmux_plugin::RenderOp::CellGrid {
                    x: 0,
                    y: 0,
                    rows: vec![vec![
                        bmux_plugin::RenderCell::sparse(RenderStyle::new()),
                        bmux_plugin::RenderCell::new('x', RenderStyle::new()),
                    ]],
                },
                clip: DamageRect::new(0, 0, 2, 1),
            }],
        };
        let spans = prepare_content_overlays(&plan).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].buffer().area(), Rect::new(1, 0, 1, 1));
        plan.overlays[0].operation = bmux_plugin::RenderOp::text_run(
            0,
            0,
            "x".repeat(16 * 1024 * 1024 + 1),
            RenderStyle::new(),
        );
        assert!(matches!(
            prepare_content_overlays(&plan),
            Err(ProjectionFailure::Budget { .. })
        ));
    }

    #[test]
    fn fill_and_border_preserve_original_sparse_interior() {
        use crate::{
            compositor::{ContentPaintOverlay, ContentPaintPlan},
            render::DamageRect,
        };
        let plan = ContentPaintPlan {
            clips: vec![DamageRect::new(0, 0, 4, 3)],
            overlays: vec![ContentPaintOverlay {
                surface_id: uuid::Uuid::nil(),
                owner: None,
                revision: None,
                operation_index: 0,
                operation: bmux_plugin::RenderOp::Border {
                    rect: bmux_plugin::ExtensionRect {
                        x: 0,
                        y: 0,
                        w: 4,
                        h: 3,
                    },
                    glyphs: bmux_plugin::BorderGlyphs::ascii(),
                    style: RenderStyle::new(),
                },
                clip: DamageRect::new(0, 0, 4, 3),
            }],
        };
        let spans = prepare_content_overlays(&plan).unwrap();
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].buffer().row_symbols(0).unwrap(), "+--+");
        assert_eq!(spans[1].buffer().area(), Rect::new(0, 1, 1, 1));
        assert_eq!(spans[2].buffer().area(), Rect::new(3, 1, 1, 1));
        assert!(spans.iter().all(|span| !span.paint_background()));
        let mut fill = plan;
        fill.overlays[0].operation = bmux_plugin::RenderOp::FillRect {
            rect: bmux_plugin::ExtensionRect {
                x: 0,
                y: 0,
                w: 4,
                h: 3,
            },
            ch: ' ',
            style: RenderStyle::new().rgb_background(12, 34, 56),
        };
        let spans = prepare_content_overlays(&fill).unwrap();
        assert_eq!(spans.len(), 3);
        assert!(
            spans
                .iter()
                .all(bmux_tui::presentation::PreparedGridOverlay::paint_background)
        );
        fill.overlays[0].operation = bmux_plugin::RenderOp::FillRect {
            rect: bmux_plugin::ExtensionRect {
                x: 0,
                y: 0,
                w: 4,
                h: 3,
            },
            ch: '界',
            style: RenderStyle::new(),
        };
        assert!(matches!(
            prepare_content_overlays(&fill),
            Err(ProjectionFailure::Unsupported { .. })
        ));
    }

    #[test]
    fn capture_grids_require_explicit_retained_admission() {
        use bmux_tui::{
            buffer::Buffer,
            damage::Damage,
            geometry::Point,
            presentation::{GridTransform, PreparedGridFragment, PresentationState},
        };
        let fragment = PreparedGridFragment::new(
            "content",
            std::sync::Arc::new(Buffer::empty(Rect::new(0, 0, 2, 1))),
            GridTransform::new(Point::new(0, 0), 500).unwrap(),
            vec![Rect::new(0, 0, 1, 1)],
        )
        .unwrap();
        let ((), operations) =
            capture_neutral(|| record(PaintOperation::Grid(std::sync::Arc::new(fragment.clone()))));
        let state = PresentationState::new(Rect::new(0, 0, 2, 1));
        state
            .prepare(Damage::Full, |cx| {
                assert!(matches!(
                    paint(cx, &operations),
                    Err(ProjectionFailure::Unsupported { .. })
                ));
                assert_eq!(
                    paint_with_grid_fragments(cx, &operations).unwrap(),
                    vec![fragment]
                );
            })
            .unwrap();
    }

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
            fit: bmux_tui::image::ImageFit::Stretch,
            allocation: None,
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
