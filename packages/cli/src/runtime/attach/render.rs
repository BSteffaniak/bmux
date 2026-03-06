use super::layout::collect_layout_rects;
use super::state::{AttachCursorState, PaneRect, PaneRenderBuffer};
use anyhow::{Context, Result};
use bmux_ipc::PaneLayoutNode;
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

pub(crate) fn append_pane_output(buffer: &mut PaneRenderBuffer, bytes: &[u8]) {
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

pub(crate) fn render_attach_panes(
    stdout: &mut io::Stdout,
    layout: &PaneLayoutNode,
    focused_pane_id: Uuid,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    dirty_pane_ids: &BTreeSet<Uuid>,
    full_pane_redraw: bool,
) -> Result<Option<AttachCursorState>> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows <= 1 {
        return Ok(None);
    }

    let draw_rows = rows.saturating_sub(1);
    let root = PaneRect {
        x: 0,
        y: 1,
        w: cols,
        h: draw_rows,
    };

    let mut rects = BTreeMap::new();
    collect_layout_rects(layout, root, &mut rects);

    let mut cursor_state = None;
    if full_pane_redraw {
        for y in 1..rows {
            queue!(stdout, MoveTo(0, y), Print(" ".repeat(usize::from(cols))))
                .context("failed clearing attach pane row")?;
        }
    }

    for (pane_id, rect) in rects {
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let should_draw = full_pane_redraw || dirty_pane_ids.contains(&pane_id);
        let focus = pane_id == focused_pane_id;
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
            let screen = entry.parser.screen();
            if pane_id == focused_pane_id {
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
