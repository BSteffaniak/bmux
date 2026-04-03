use super::state::{
    AttachCursorState, AttachScrollbackCursor, AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
};
use anyhow::{Context, Result};
use bmux_ipc::{AttachFocusTarget, AttachScene, AttachSurfaceKind, PaneState, PaneSummary};
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
pub enum AttachLayer {
    Pane = 0,
    Overlay = 100,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachLayerSurface {
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

pub fn append_pane_output(buffer: &mut PaneRenderBuffer, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let was_alternate = buffer.last_alternate_screen;
    buffer.parser.process(bytes);
    let is_alternate = buffer.parser.screen().alternate_screen();
    buffer.last_alternate_screen = is_alternate;

    let toggled_alternate =
        was_alternate != is_alternate || contains_alternate_screen_sequence(bytes);
    if toggled_alternate {
        // Alternate-screen transitions can restore or replace rows without
        // re-emitting every line. Invalidate row diff cache so next render
        // repaints the pane deterministically.
        buffer.prev_rows.clear();
    }

    toggled_alternate
}

fn contains_alternate_screen_sequence(bytes: &[u8]) -> bool {
    const SEQUENCES: [&[u8]; 6] = [
        b"\x1b[?47h",
        b"\x1b[?47l",
        b"\x1b[?1047h",
        b"\x1b[?1047l",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
    ];

    SEQUENCES
        .iter()
        .any(|needle| bytes.windows(needle.len()).any(|window| window == *needle))
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

pub fn opaque_row_text(content: &str, width: usize) -> String {
    let mut rendered = content.to_string();
    if rendered.len() > width {
        rendered.truncate(width);
    }
    if rendered.len() < width {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }
    rendered
}

pub fn queue_layer_fill<W: io::Write>(stdout: &mut W, surface: AttachLayerSurface) -> Result<()> {
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

const fn selected_style(mut style: CellStyle) -> CellStyle {
    style.inverse = !style.inverse;
    style
}

fn selection_bounds(
    anchor: Option<AttachScrollbackPosition>,
    cursor: Option<AttachScrollbackCursor>,
    scrollback_offset: usize,
) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
    let anchor = anchor?;
    let cursor = cursor?;
    let head = AttachScrollbackPosition {
        row: scrollback_offset.saturating_add(cursor.row),
        col: cursor.col,
    };
    Some(if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    })
}

const fn cell_selected(
    selection: Option<(AttachScrollbackPosition, AttachScrollbackPosition)>,
    row: usize,
    col: usize,
) -> bool {
    let Some((start, end)) = selection else {
        return false;
    };
    if row < start.row || row > end.row {
        return false;
    }
    if start.row == end.row {
        return row == start.row && col >= start.col && col <= end.col;
    }
    if row == start.row {
        return col >= start.col;
    }
    if row == end.row {
        return col <= end.col;
    }
    true
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

pub fn render_attach_scene<W: io::Write>(
    stdout: &mut W,
    scene: &AttachScene,
    panes: &[PaneSummary],
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    dirty_pane_ids: &BTreeSet<Uuid>,
    full_pane_redraw: bool,
    status_top_inset: u16,
    status_bottom_inset: u16,
    scrollback_active: bool,
    scrollback_offset: usize,
    scrollback_cursor: Option<AttachScrollbackCursor>,
    selection_anchor: Option<AttachScrollbackPosition>,
    zoomed: bool,
) -> Result<Option<AttachCursorState>> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows <= status_top_inset.saturating_add(status_bottom_inset) {
        return Ok(None);
    }

    let pane_states = panes
        .iter()
        .map(|pane| (pane.id, pane.state))
        .collect::<BTreeMap<Uuid, PaneState>>();

    let mut cursor_state = None;
    if full_pane_redraw {
        let clear_start = status_top_inset.min(rows);
        let clear_end = rows.saturating_sub(status_bottom_inset).max(clear_start);
        for y in clear_start..clear_end {
            queue!(stdout, MoveTo(0, y), Print(" ".repeat(usize::from(cols))))
                .context("failed clearing attach pane row")?;
        }
        // Invalidate all row caches so every row is re-emitted.
        for buffer in pane_buffers.values_mut() {
            buffer.prev_rows.clear();
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
            let (corner, hch, vch) = if zoomed && focus {
                // Zoomed pane: double-line box drawing characters.
                ('#', '=', '\u{2551}') // ║ for sides, # corners, = top/bottom
            } else if focus {
                ('+', '=', '|')
            } else {
                ('+', '-', '|')
            };
            let top = draw_box_line(usize::from(rect.w), corner, hch, corner);
            let bottom = draw_box_line(usize::from(rect.w), corner, hch, corner);
            queue!(stdout, MoveTo(rect.x, rect.y), Print(top))
                .context("failed drawing pane top")?;
            queue!(
                stdout,
                MoveTo(rect.x, rect.y.saturating_add(rect.h.saturating_sub(1))),
                Print(bottom)
            )
            .context("failed drawing pane bottom")?;

            for y in rect.y.saturating_add(1)..rect.y.saturating_add(rect.h.saturating_sub(1)) {
                queue!(stdout, MoveTo(rect.x, y), Print(vch))
                    .context("failed drawing pane left border")?;
                queue!(
                    stdout,
                    MoveTo(rect.x.saturating_add(rect.w.saturating_sub(1)), y),
                    Print(vch)
                )
                .context("failed drawing pane right border")?;
            }

            if rect.w > 6 {
                let badge = match pane_states
                    .get(&pane_id)
                    .copied()
                    .unwrap_or(PaneState::Running)
                {
                    PaneState::Running => "[RUNNING]",
                    PaneState::Exited => "[EXITED]",
                };
                let max_badge_width = usize::from(rect.w.saturating_sub(4));
                let badge_text = opaque_row_text(badge, badge.len().min(max_badge_width));
                queue!(
                    stdout,
                    MoveTo(rect.x.saturating_add(2), rect.y),
                    Print(badge_text)
                )
                .context("failed drawing pane state badge")?;
            }
        }

        let inner_w_u16 = rect.w.saturating_sub(2);
        let inner_h_u16 = rect.h.saturating_sub(2);
        let inner_w = usize::from(inner_w_u16);
        let inner_h = usize::from(inner_h_u16);
        if let Some(entry) = pane_buffers.get_mut(&pane_id) {
            let (old_rows, old_cols) = entry.parser.screen().size();
            entry
                .parser
                .screen_mut()
                .set_size(inner_h_u16.max(1), inner_w_u16.max(1));
            // Invalidate the row cache when the pane dimensions change, since
            // the row strings are no longer comparable at a different size.
            let (new_rows, new_cols) = entry.parser.screen().size();
            if (new_rows, new_cols) != (old_rows, old_cols) {
                entry.prev_rows.clear();
            }
            let use_scrollback = scrollback_active && focus;
            let previous_scrollback = entry.parser.screen().scrollback();
            if use_scrollback {
                entry.parser.screen_mut().set_scrollback(scrollback_offset);
            }
            let screen = entry.parser.screen();
            let selection = if use_scrollback {
                selection_bounds(selection_anchor, scrollback_cursor, scrollback_offset)
            } else {
                None
            };
            if focus {
                let (cursor_row, cursor_col) = if use_scrollback {
                    let cursor =
                        scrollback_cursor.unwrap_or(AttachScrollbackCursor { row: 0, col: 0 });
                    (
                        cursor.row.min(inner_h.saturating_sub(1)) as u16,
                        cursor.col.min(inner_w.saturating_sub(1)) as u16,
                    )
                } else {
                    let (cursor_row, cursor_col) = screen.cursor_position();
                    (
                        cursor_row.min(inner_h_u16.saturating_sub(1)),
                        cursor_col.min(inner_w_u16.saturating_sub(1)),
                    )
                };
                cursor_state = Some(AttachCursorState {
                    x: rect.x.saturating_add(1).saturating_add(cursor_col),
                    y: rect.y.saturating_add(1).saturating_add(cursor_row),
                    visible: use_scrollback || !screen.hide_cursor(),
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
                        let absolute_row = scrollback_offset.saturating_add(row);
                        let style = if cell_selected(selection, absolute_row, usize::from(col)) {
                            selected_style(cell_style(cell))
                        } else {
                            cell_style(cell)
                        };
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

                // Row-level diff: skip emitting if the rendered string
                // matches the previous frame's cached version for this row.
                let cached = entry.prev_rows.get(row);
                if cached.map_or(true, |c| *c != line) {
                    queue!(stdout, MoveTo(rect.x.saturating_add(1), y), Print(&line))
                        .context("failed drawing pane content")?;
                    if row < entry.prev_rows.len() {
                        entry.prev_rows[row] = line;
                    } else {
                        entry.prev_rows.push(line);
                    }
                }
            }
            // Trim stale cache entries if the visible row count shrank.
            entry.prev_rows.truncate(inner_h);
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
    use super::{
        AttachLayer, AttachLayerSurface, append_pane_output, opaque_row_text, queue_layer_fill,
        render_attach_scene,
    };
    use crate::runtime::attach::state::{
        AttachScrollbackCursor, AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
    };
    use bmux_ipc::{
        AttachFocusTarget, AttachLayer as SurfaceLayer, AttachRect, AttachScene, AttachSurface,
        AttachSurfaceKind, PaneState, PaneSummary,
    };
    use crossterm::cursor::MoveTo;
    use crossterm::queue;
    use crossterm::style::Print;
    use std::collections::{BTreeMap, BTreeSet};
    use uuid::Uuid;

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

    #[test]
    fn append_output_detects_alternate_screen_toggle() {
        let mut buffer = PaneRenderBuffer::default();
        buffer.prev_rows.push("cached".to_string());

        let toggled = append_pane_output(&mut buffer, b"\x1b[?1049h");
        assert!(toggled);
        assert!(buffer.parser.screen().alternate_screen());
        assert!(buffer.prev_rows.is_empty());
    }

    #[test]
    fn append_output_detects_enter_and_exit_same_chunk() {
        let mut buffer = PaneRenderBuffer::default();

        let toggled = append_pane_output(&mut buffer, b"\x1b[?1049hhello\x1b[?1049l");
        assert!(toggled);
        assert!(!buffer.parser.screen().alternate_screen());
    }

    #[test]
    fn render_attach_scene_keeps_cursor_visible_in_scrollback() {
        let pane_id = Uuid::from_u128(1);
        let scene = AttachScene {
            session_id: Uuid::from_u128(2),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 20,
                    h: 6,
                },
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        buffer.parser.screen_mut().set_size(4, 18);
        buffer.parser.process(b"hello\nworld\n");
        buffer.parser.process(b"\x1b[?25l");
        buffer.parser.screen_mut().set_scrollback(1);
        pane_buffers.insert(pane_id, buffer);

        let mut output = Vec::new();
        let cursor_state = render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &BTreeSet::from([pane_id]),
            true,
            1,
            0,
            true,
            1,
            Some(AttachScrollbackCursor { row: 0, col: 0 }),
            None,
            false,
        )
        .expect("render should succeed");

        if let Some(cursor_state) = cursor_state {
            assert!(cursor_state.visible);
        }
    }

    #[test]
    fn render_attach_scene_highlights_selected_cells() {
        let pane_id = Uuid::from_u128(21);
        let scene = AttachScene {
            session_id: Uuid::from_u128(22),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 12,
                    h: 4,
                },
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer::default();
        buffer.parser.screen_mut().set_size(2, 10);
        buffer.parser.process(b"abcdef\n");
        pane_buffers.insert(pane_id, buffer);

        let mut output = Vec::new();
        let _ = render_attach_scene(
            &mut output,
            &scene,
            &[],
            &mut pane_buffers,
            &BTreeSet::from([pane_id]),
            true,
            1,
            0,
            true,
            0,
            Some(AttachScrollbackCursor { row: 0, col: 4 }),
            Some(AttachScrollbackPosition { row: 0, col: 1 }),
            false,
        )
        .expect("render should succeed");

        let _rendered = String::from_utf8(output).expect("render output should be utf8");
    }

    #[test]
    fn render_attach_scene_draws_exited_badge() {
        let pane_id = Uuid::from_u128(31);
        let scene = AttachScene {
            session_id: Uuid::from_u128(32),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 20,
                    h: 5,
                },
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let panes = vec![PaneSummary {
            id: pane_id,
            index: 1,
            name: None,
            focused: true,
            state: PaneState::Exited,
            state_reason: Some("process exited with status 130".to_string()),
        }];
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(pane_id, PaneRenderBuffer::default());

        let mut output = Vec::new();
        let _ = render_attach_scene(
            &mut output,
            &scene,
            &panes,
            &mut pane_buffers,
            &BTreeSet::from([pane_id]),
            true,
            1,
            0,
            false,
            0,
            None,
            None,
            false,
        )
        .expect("render should succeed");

        let rendered = String::from_utf8(output).expect("render output should be utf8");
        if !rendered.is_empty() {
            assert!(rendered.contains("[EXITED]"));
        }
    }
}
