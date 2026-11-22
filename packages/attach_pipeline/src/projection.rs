//! Retained paint operations and the failures that keep a frame from being shown.
//!
//! The attach renderer paints straight into a frame (see [`crate::canvas`]); operations are only
//! kept where output is retained across frames, an extension's output replayed while it is
//! unchanged.
use bmux_plugin::{RenderColor, RenderNamedColor, RenderStyle};
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span};

/// One retained, already clipped operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaintOperation {
    /// The output cannot be shown.
    Rejected(ProjectionFailure),
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
            RenderOp::Cells(block) => {
                for row_index in 0..block.height {
                    let Some(y) = block.y.checked_add(row_index) else {
                        break;
                    };
                    for column in 0..block.width {
                        let Some(x) = block.x.checked_add(column) else {
                            break;
                        };
                        let Some(symbol) = block
                            .get(column, row_index)
                            .and_then(|cell| cell.symbol.as_ref())
                        else {
                            continue;
                        };
                        let mut buffer = [0; 4];
                        let style = block
                            .get(column, row_index)
                            .map_or_else(RenderStyle::new, |cell| cell.style);
                        row(x, y, symbol.as_str(&mut buffer), style)?;
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
}
