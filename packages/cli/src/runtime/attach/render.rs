use super::state::{AttachCursorState, PaneRect, PaneRenderBuffer};
use anyhow::{Context, Result};
use bmux_ipc::{AttachFocusTarget, AttachScene, AttachSurfaceKind};
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AttachLayer {
    Pane = 0,
    Overlay = 100,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttachLayerSurface {
    pub(crate) rect: PaneRect,
    pub(crate) layer: AttachLayer,
    pub(crate) opaque: bool,
}

impl AttachLayerSurface {
    pub(crate) const fn new(rect: PaneRect, layer: AttachLayer, opaque: bool) -> Self {
        Self {
            rect,
            layer,
            opaque,
        }
    }
}

pub fn append_pane_output(buffer: &mut PaneRenderBuffer, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    buffer.parser.process(bytes);
}

fn draw_box_line(width: usize, left: char, mid: char, right: char) -> String {
    if width <= 1 {
        return left.to_string();
    }
    let mut line = String::new();
    line.push(left);
    if width > 2 {
        line.extend(std::iter::repeat_n(mid, width - 2));
    }
    line.push(right);
    line
}

pub(crate) fn opaque_row_text(content: &str, width: usize) -> String {
    let mut rendered = content.to_string();
    if rendered.len() > width {
        rendered.truncate(width);
    }
    if rendered.len() < width {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }
    rendered
}

pub(crate) fn queue_layer_fill<W: io::Write>(
    stdout: &mut W,
    surface: AttachLayerSurface,
) -> Result<()> {
    if !surface.opaque || surface.rect.w <= 2 || surface.rect.h <= 2 {
        return Ok(());
    }

    let fill = " ".repeat(usize::from(surface.rect.w.saturating_sub(2)));
    for y in surface.rect.y.saturating_add(1)
        ..surface
            .rect
            .y
            .saturating_add(surface.rect.h.saturating_sub(1))
    {
        queue!(
            stdout,
            MoveTo(surface.rect.x.saturating_add(1), y),
            Print(&fill)
        )
        .with_context(|| format!("failed filling {:?} layer row", surface.layer))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct CellStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

fn cell_style(cell: &vt100::Cell) -> CellStyle {
    CellStyle {
        fg: cell.fgcolor(),
        bg: cell.bgcolor(),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
    }
}

fn color_sgr(color: vt100::Color, foreground: bool) -> String {
    match color {
        vt100::Color::Default => {
            if foreground {
                "39".to_string()
            } else {
                "49".to_string()
            }
        }
        vt100::Color::Idx(idx) => {
            if foreground {
                format!("38;5;{idx}")
            } else {
                format!("48;5;{idx}")
            }
        }
        vt100::Color::Rgb(r, g, b) => {
            if foreground {
                format!("38;2;{r};{g};{b}")
            } else {
                format!("48;2;{r};{g};{b}")
            }
        }
    }
}

fn style_sgr(style: CellStyle) -> String {
    let mut parts = vec!["0".to_string()];
    if style.bold {
        parts.push("1".to_string());
    }
    if style.dim {
        parts.push("2".to_string());
    }
    if style.italic {
        parts.push("3".to_string());
    }
    if style.underline {
        parts.push("4".to_string());
    }
    if style.inverse {
        parts.push("7".to_string());
    }
    parts.push(color_sgr(style.fg, true));
    parts.push(color_sgr(style.bg, false));
    format!("\x1b[{}m", parts.join(";"))
}

pub fn visible_scene_pane_ids(scene: &AttachScene) -> Vec<Uuid> {
    let mut pane_ids = BTreeSet::new();
    for surface in &scene.surfaces {
        if surface.visible
            && let Some(pane_id) = surface.pane_id
        {
            pane_ids.insert(pane_id);
        }
    }
    pane_ids.into_iter().collect()
}

pub fn render_attach_scene(
    stdout: &mut io::Stdout,
    scene: &AttachScene,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    dirty_pane_ids: &BTreeSet<Uuid>,
    full_pane_redraw: bool,
    scrollback_active: bool,
    scrollback_offset: usize,
) -> Result<Option<AttachCursorState>> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows <= 1 {
        return Ok(None);
    }

    let mut cursor_state = None;
    if full_pane_redraw {
        for y in 1..rows {
            queue!(stdout, MoveTo(0, y), Print(" ".repeat(usize::from(cols))))
                .context("failed clearing attach pane row")?;
        }
    }

    let focused_surface_id = match scene.focus {
        AttachFocusTarget::Surface { surface_id } => Some(surface_id),
        _ => None,
    };
    let focused_pane_id = match scene.focus {
        AttachFocusTarget::Pane { pane_id } => Some(pane_id),
        _ => None,
    };

    let mut ordered_surfaces = scene.surfaces.iter().enumerate().collect::<Vec<_>>();
    ordered_surfaces.sort_by_key(|(index, surface)| (surface.layer, surface.z, *index));

    for (_index, surface) in ordered_surfaces {
        if !surface.visible {
            continue;
        }
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !matches!(
            surface.kind,
            AttachSurfaceKind::Pane | AttachSurfaceKind::FloatingPane
        ) {
            continue;
        }
        let rect = PaneRect {
            x: surface.rect.x,
            y: surface.rect.y,
            w: surface.rect.w,
            h: surface.rect.h,
        };
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let should_draw = full_pane_redraw || dirty_pane_ids.contains(&pane_id);
        let focus = surface.cursor_owner
            || focused_surface_id == Some(surface.id)
            || focused_pane_id == Some(pane_id);
        if should_draw {
            let hch = if focus { '=' } else { '-' };
            let top = draw_box_line(usize::from(rect.w), '+', hch, '+');
            let bottom = draw_box_line(usize::from(rect.w), '+', hch, '+');
            queue!(stdout, MoveTo(rect.x, rect.y), Print(top))
                .context("failed drawing pane top")?;
            queue!(
                stdout,
                MoveTo(rect.x, rect.y.saturating_add(rect.h.saturating_sub(1))),
                Print(bottom)
            )
            .context("failed drawing pane bottom")?;

            for y in rect.y.saturating_add(1)..rect.y.saturating_add(rect.h.saturating_sub(1)) {
                queue!(stdout, MoveTo(rect.x, y), Print("|"))
                    .context("failed drawing pane left border")?;
                queue!(
                    stdout,
                    MoveTo(rect.x.saturating_add(rect.w.saturating_sub(1)), y),
                    Print("|")
                )
                .context("failed drawing pane right border")?;
            }
        }

        let inner_w_u16 = rect.w.saturating_sub(2);
        let inner_h_u16 = rect.h.saturating_sub(2);
        let inner_w = usize::from(inner_w_u16);
        let inner_h = usize::from(inner_h_u16);
        if let Some(entry) = pane_buffers.get_mut(&pane_id) {
            entry
                .parser
                .screen_mut()
                .set_size(inner_h_u16.max(1), inner_w_u16.max(1));
            let use_scrollback = scrollback_active && focus;
            let previous_scrollback = entry.parser.screen().scrollback();
            if use_scrollback {
                entry.parser.screen_mut().set_scrollback(scrollback_offset);
            }
            let screen = entry.parser.screen();
            if focus && !use_scrollback {
                let (cursor_row, cursor_col) = screen.cursor_position();
                let cursor_row = cursor_row.min(inner_h_u16.saturating_sub(1));
                let cursor_col = cursor_col.min(inner_w_u16.saturating_sub(1));
                cursor_state = Some(AttachCursorState {
                    x: rect.x.saturating_add(1).saturating_add(cursor_col),
                    y: rect.y.saturating_add(1).saturating_add(cursor_row),
                    visible: !screen.hide_cursor(),
                });
            }
            if !should_draw {
                continue;
            }
            for row in 0..inner_h {
                let y = rect.y.saturating_add(1 + row as u16);
                let mut line = String::new();
                let mut current = CellStyle::default();
                let mut used_cols = 0usize;
                let mut col = 0u16;
                while col < inner_w_u16 {
                    if let Some(cell) = screen.cell(row as u16, col) {
                        let style = cell_style(cell);
                        if style != current {
                            line.push_str(&style_sgr(style));
                            current = style;
                        }
                        if cell.is_wide_continuation() {
                            line.push(' ');
                            used_cols = used_cols.saturating_add(1);
                            col = col.saturating_add(1);
                            continue;
                        }
                        let text = if cell.has_contents() {
                            cell.contents()
                        } else {
                            " "
                        };
                        line.push_str(text);
                        let width = UnicodeWidthStr::width(text).max(1);
                        used_cols = used_cols.saturating_add(width);
                        if cell.is_wide() {
                            col = col.saturating_add(2);
                        } else {
                            col = col.saturating_add(1);
                        }
                    } else {
                        if current != CellStyle::default() {
                            line.push_str("\x1b[0m");
                            current = CellStyle::default();
                        }
                        line.push(' ');
                        used_cols = used_cols.saturating_add(1);
                        col = col.saturating_add(1);
                    }
                }

                if used_cols < inner_w {
                    if current != CellStyle::default() {
                        line.push_str("\x1b[0m");
                    }
                    line.push_str(&" ".repeat(inner_w - used_cols));
                } else if current != CellStyle::default() {
                    line.push_str("\x1b[0m");
                }

                queue!(stdout, MoveTo(rect.x.saturating_add(1), y), Print(line))
                    .context("failed drawing pane content")?;
            }
            if use_scrollback {
                entry
                    .parser
                    .screen_mut()
                    .set_scrollback(previous_scrollback);
            }
        } else if should_draw {
            for row in 0..inner_h {
                let y = rect.y.saturating_add(1 + row as u16);
                queue!(
                    stdout,
                    MoveTo(rect.x.saturating_add(1), y),
                    Print(" ".repeat(inner_w))
                )
                .context("failed clearing pane content")?;
            }
        }
    }

    Ok(cursor_state)
}

#[cfg(test)]
mod tests {
    use super::{AttachLayer, AttachLayerSurface, opaque_row_text, queue_layer_fill};
    use crate::runtime::attach::state::PaneRect;
    use crossterm::cursor::MoveTo;
    use crossterm::queue;
    use crossterm::style::Print;

    fn screen_row(screen: &vt100::Screen, row: u16, width: u16) -> String {
        let mut line = String::new();
        for col in 0..width {
            let cell = screen.cell(row, col).expect("screen cell should exist");
            line.push_str(if cell.has_contents() {
                cell.contents()
            } else {
                " "
            });
        }
        line
    }

    #[test]
    fn opaque_row_text_truncates_and_pads() {
        assert_eq!(opaque_row_text("help", 8), "help    ");
        assert_eq!(opaque_row_text("123456789", 5), "12345");
    }

    #[test]
    fn queue_layer_fill_and_text_overwrite_existing_content() {
        let mut parser = vt100::Parser::new(6, 20, 128);
        parser.process(b"\x1b[2;1H0123456789abcdefghij");

        let surface = AttachLayerSurface::new(
            PaneRect {
                x: 0,
                y: 0,
                w: 12,
                h: 4,
            },
            AttachLayer::Overlay,
            true,
        );

        let mut bytes = Vec::new();
        queue_layer_fill(&mut bytes, surface).expect("overlay fill should succeed");
        queue!(
            bytes,
            MoveTo(1, 1),
            Print(opaque_row_text(
                "help",
                usize::from(surface.rect.w.saturating_sub(2))
            ))
        )
        .expect("overlay text should queue");

        parser.process(&bytes);

        assert_eq!(screen_row(parser.screen(), 1, 12), "0help      b");
    }
}
