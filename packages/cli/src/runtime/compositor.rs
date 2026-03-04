use super::PaneRuntime;
use crate::cli::DebugRenderLogFormat;
use crate::pane::{PaneId, Rect};
use crate::status::{build_status_line, write_status_line};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vt100::{Color, Screen};

#[derive(Default)]
pub(super) struct RenderCache {
    initialized: bool,
    status_line: String,
    pane_rects: BTreeMap<PaneId, Rect>,
    pane_titles: BTreeMap<PaneId, String>,
    focused_pane: Option<PaneId>,
    pane_lines: BTreeMap<PaneId, Vec<Vec<u8>>>,
}

struct PaneRenderData {
    pane_id: PaneId,
    rect: Rect,
    title: String,
    lines: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SelectionOverlay {
    pub(super) pane_id: PaneId,
    pub(super) start_row: u16,
    pub(super) start_col: u16,
    pub(super) end_row: u16,
    pub(super) end_col: u16,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CursorOverlay {
    pub(super) pane_id: PaneId,
    pub(super) row: u16,
    pub(super) col: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CellStyle {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

pub(super) struct RenderDebugState {
    enabled: bool,
    window_start: Instant,
    frames: u32,
    changed_lines: usize,
    status_updates: u32,
    border_updates: u32,
    snapshot: String,
    logger: Option<RenderDebugLogger>,
}

struct RenderDebugLogger {
    file: std::fs::File,
    started_at: Instant,
    format: DebugRenderLogFormat,
}

impl RenderDebugState {
    pub(super) fn new(
        enabled: bool,
        log_path: Option<&Path>,
        log_format: DebugRenderLogFormat,
    ) -> Result<Self> {
        let logger = if let Some(path) = log_path {
            Some(RenderDebugLogger::new(path, log_format)?)
        } else {
            None
        };

        Ok(Self {
            enabled,
            window_start: Instant::now(),
            frames: 0,
            changed_lines: 0,
            status_updates: 0,
            border_updates: 0,
            snapshot: String::new(),
            logger,
        })
    }

    fn record_frame(&mut self, changed_lines: usize, status_updated: bool, border_updated: bool) {
        if let Some(logger) = self.logger.as_mut() {
            let elapsed_ms = logger.started_at.elapsed().as_millis();
            let _ = logger.write_frame(elapsed_ms, changed_lines, status_updated, border_updated);
        }

        if !self.enabled {
            return;
        }

        self.frames = self.frames.saturating_add(1);
        self.changed_lines = self.changed_lines.saturating_add(changed_lines);
        if status_updated {
            self.status_updates = self.status_updates.saturating_add(1);
        }
        if border_updated {
            self.border_updates = self.border_updates.saturating_add(1);
        }

        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let fps = f64::from(self.frames) / elapsed.as_secs_f64();
            let lines_per_frame = if self.frames == 0 {
                0.0
            } else {
                self.changed_lines as f64 / f64::from(self.frames)
            };

            self.snapshot = format!(
                "render: {fps:.1}fps | lines/frame: {lines_per_frame:.1} | status: {} | borders: {}",
                self.status_updates, self.border_updates
            );

            if let Some(logger) = self.logger.as_mut() {
                let elapsed_ms = logger.started_at.elapsed().as_millis();
                let _ = logger.write_window(
                    elapsed_ms,
                    fps,
                    lines_per_frame,
                    self.status_updates,
                    self.border_updates,
                );
            }

            self.frames = 0;
            self.changed_lines = 0;
            self.status_updates = 0;
            self.border_updates = 0;
            self.window_start = Instant::now();
        }
    }

    pub(super) fn snapshot(&self) -> Option<&str> {
        if self.enabled {
            Some(&self.snapshot)
        } else {
            None
        }
    }
}

impl RenderDebugLogger {
    fn new(path: &Path, format: DebugRenderLogFormat) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed opening render debug log at {}", path.display()))?;

        match format {
            DebugRenderLogFormat::Text => {
                let _ = writeln!(file, "# bmux render debug log");
            }
            DebugRenderLogFormat::Csv => {
                let _ = writeln!(
                    file,
                    "event,t_ms,changed_lines,status_updated,border_updated,fps,lines_per_frame,status_updates,border_updates"
                );
            }
        }

        Ok(Self {
            file,
            started_at: Instant::now(),
            format,
        })
    }

    fn write_frame(
        &mut self,
        elapsed_ms: u128,
        changed_lines: usize,
        status_updated: bool,
        border_updated: bool,
    ) -> std::io::Result<()> {
        match self.format {
            DebugRenderLogFormat::Text => writeln!(
                self.file,
                "t={}ms frame changed_lines={} status_updated={} border_updated={}",
                elapsed_ms, changed_lines, status_updated, border_updated
            ),
            DebugRenderLogFormat::Csv => writeln!(
                self.file,
                "frame,{},{},{},{},,,,",
                elapsed_ms, changed_lines, status_updated, border_updated
            ),
        }
    }

    fn write_window(
        &mut self,
        elapsed_ms: u128,
        fps: f64,
        lines_per_frame: f64,
        status_updates: u32,
        border_updates: u32,
    ) -> std::io::Result<()> {
        match self.format {
            DebugRenderLogFormat::Text => writeln!(
                self.file,
                "t={}ms window fps={:.3} lines_per_frame={:.3} status_updates={} border_updates={}",
                elapsed_ms, fps, lines_per_frame, status_updates, border_updates
            ),
            DebugRenderLogFormat::Csv => writeln!(
                self.file,
                "window,{},,,,,{:.3},{:.3},{},{}",
                elapsed_ms, fps, lines_per_frame, status_updates, border_updates
            ),
        }
    }
}

pub(super) fn render_frame(
    panes: &BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
    cols: u16,
    rows: u16,
    shell_name: &str,
    cwd: &Path,
    focused_pane: PaneId,
    show_terminal_cursor: bool,
    cursor: Option<CursorOverlay>,
    selection: Option<SelectionOverlay>,
    mode_suffix: Option<&str>,
    status_message: Option<&str>,
    full_redraw: bool,
    render_cache: &mut RenderCache,
    render_debug: &mut RenderDebugState,
) -> Result<()> {
    let debug_snapshot = render_debug.snapshot();
    let mut status_parts = Vec::new();
    if let Some(mode) = mode_suffix {
        status_parts.push(mode.to_string());
    }
    if let Some(message) = status_message {
        status_parts.push(message.to_string());
    }
    if let Some(debug) = debug_snapshot {
        status_parts.push(debug.to_string());
    }
    let status_suffix = if status_parts.is_empty() {
        None
    } else {
        Some(status_parts.join(" | "))
    };

    let mut pane_data = Vec::new();
    for (pane_id, rect) in pane_rects {
        if let Some(pane) = panes.get(pane_id) {
            pane_data.push(collect_pane_render_data(
                *pane_id,
                pane,
                *rect,
                *pane_id == focused_pane,
                cursor.filter(|overlay| overlay.pane_id == *pane_id),
                selection.filter(|overlay| overlay.pane_id == *pane_id),
            ));
        }
    }
    let focused_index = pane_data
        .iter()
        .position(|pane| pane.pane_id == focused_pane)
        .unwrap_or(0);

    let status_line = build_status_line(
        shell_name,
        cwd,
        cols,
        rows,
        focused_index,
        status_suffix.as_deref(),
    );

    let mut stdout = io::stdout();
    write!(stdout, "\x1b[?25l").context("failed hiding cursor")?;

    let status_changed =
        full_redraw || !render_cache.initialized || render_cache.status_line != status_line;
    if status_changed {
        write_status_line(&status_line, cols).context("failed drawing status line")?;
        render_cache.status_line = status_line;
    }

    let border_changed = full_redraw
        || !render_cache.initialized
        || render_cache.focused_pane != Some(focused_pane)
        || render_cache.pane_rects != *pane_rects
        || pane_data
            .iter()
            .any(|pane| render_cache.pane_titles.get(&pane.pane_id) != Some(&pane.title));

    if border_changed {
        clear_body_region(&mut stdout, cols, rows)?;
    }

    let mut changed_line_count = 0_usize;
    for pane in &pane_data {
        changed_line_count += draw_changed_lines(
            &mut stdout,
            pane,
            render_cache
                .pane_lines
                .get(&pane.pane_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            full_redraw || border_changed,
        )?;
    }

    if border_changed || changed_line_count > 0 {
        for pane in &pane_data {
            draw_rect_border(
                &mut stdout,
                pane.rect,
                if pane.pane_id == focused_pane {
                    "\x1b[36m"
                } else {
                    "\x1b[90m"
                },
                &pane.title,
            )?;
        }
    }

    if show_terminal_cursor {
        let focused_rect = pane_rects
            .get(&focused_pane)
            .copied()
            .unwrap_or_default()
            .inner();

        let cursor_pos = {
            let parser = panes[&focused_pane]
                .state
                .parser
                .lock()
                .expect("pane parser mutex poisoned");
            parser.screen().cursor_position()
        };

        let cursor_row = focused_rect
            .y
            .saturating_add(cursor_pos.0.min(focused_rect.height.saturating_sub(1)));
        let cursor_col = focused_rect
            .x
            .saturating_add(cursor_pos.1.min(focused_rect.width.saturating_sub(1)));

        if let Some(control) = terminal_cursor_control(show_terminal_cursor, cursor_row, cursor_col)
        {
            write!(stdout, "{control}").context("failed setting cursor")?;
        }
    }
    stdout.flush().context("failed flushing rendered frame")?;

    render_cache.initialized = true;
    render_cache.focused_pane = Some(focused_pane);
    render_cache.pane_rects = pane_rects.clone();
    render_cache.pane_titles = pane_data
        .iter()
        .map(|pane| (pane.pane_id, pane.title.clone()))
        .collect();
    render_cache.pane_lines = pane_data
        .iter()
        .map(|pane| (pane.pane_id, pane.lines.clone()))
        .collect();
    render_debug.record_frame(changed_line_count, status_changed, border_changed);

    Ok(())
}

fn terminal_cursor_control(
    show_terminal_cursor: bool,
    cursor_row: u16,
    cursor_col: u16,
) -> Option<String> {
    if !show_terminal_cursor {
        return None;
    }

    Some(format!("\x1b[?25h\x1b[{cursor_row};{cursor_col}H"))
}

fn collect_pane_render_data(
    pane_id: PaneId,
    pane: &PaneRuntime,
    rect: Rect,
    focused: bool,
    cursor: Option<CursorOverlay>,
    selection: Option<SelectionOverlay>,
) -> PaneRenderData {
    let inner = rect.inner();
    let title = if pane.closed {
        format!(" {} [closed] ", pane.title)
    } else if let Some(code) = pane.exit_code {
        format!(" {} [exited {code}] ", pane.title)
    } else {
        format!(" {}{} ", pane.title, if focused { " *" } else { "" })
    };

    let mut lines = Vec::new();

    if inner.width == 0 || inner.height == 0 {
        return PaneRenderData {
            pane_id,
            rect,
            title,
            lines,
        };
    }

    if pane.closed {
        let message = "pane closed | Ctrl-A r restart".to_string();
        lines.push(pad_or_truncate(&message, usize::from(inner.width)).into_bytes());
        for _ in 1..inner.height {
            lines.push(vec![b' '; usize::from(inner.width)]);
        }
        return PaneRenderData {
            pane_id,
            rect,
            title,
            lines,
        };
    }

    if let Some(code) = pane.exit_code {
        let message = format!("pane exited ({code}) | Ctrl-A r restart");
        lines.push(pad_or_truncate(&message, usize::from(inner.width)).into_bytes());
        for _ in 1..inner.height {
            lines.push(vec![b' '; usize::from(inner.width)]);
        }
        return PaneRenderData {
            pane_id,
            rect,
            title,
            lines,
        };
    }

    let parser = pane
        .state
        .parser
        .lock()
        .expect("pane parser mutex poisoned");
    let screen = parser.screen();
    for row_index in 0..inner.height {
        lines.push(render_screen_row(
            screen,
            row_index,
            inner.width,
            cursor,
            selection,
        ));
    }

    PaneRenderData {
        pane_id,
        rect,
        title,
        lines,
    }
}

fn render_screen_row(
    screen: &Screen,
    row: u16,
    width: u16,
    cursor: Option<CursorOverlay>,
    selection: Option<SelectionOverlay>,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(usize::from(width) * 4);
    let mut current_style = CellStyle::default();
    let mut col = 0_u16;

    while col < width {
        let remaining = width - col;
        let Some(cell) = screen.cell(row, col) else {
            push_style_diff(&mut output, current_style, CellStyle::default());
            output.push(b' ');
            current_style = CellStyle::default();
            col += 1;
            continue;
        };

        if cell.is_wide_continuation() {
            col += 1;
            continue;
        }

        let mut style = CellStyle {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        };

        if let Some(selection) = selection {
            if cell_in_selection(row, col, selection) {
                style.inverse = !style.inverse;
            }
        }

        if let Some(cursor) = cursor {
            if row == cursor.row && col == cursor.col {
                style.inverse = true;
                style.underline = true;
            }
        }

        push_style_diff(&mut output, current_style, style);
        current_style = style;

        if cell.has_contents() {
            if cell.is_wide() && remaining < 2 {
                output.push(b' ');
                col += 1;
            } else {
                output.extend_from_slice(cell.contents().as_bytes());
                col += if cell.is_wide() { 2 } else { 1 };
            }
        } else {
            output.push(b' ');
            col += 1;
        }
    }

    push_style_diff(&mut output, current_style, CellStyle::default());
    output
}

fn cell_in_selection(row: u16, col: u16, selection: SelectionOverlay) -> bool {
    let (start_row, start_col, end_row, end_col) =
        if (selection.start_row, selection.start_col) <= (selection.end_row, selection.end_col) {
            (
                selection.start_row,
                selection.start_col,
                selection.end_row,
                selection.end_col,
            )
        } else {
            (
                selection.end_row,
                selection.end_col,
                selection.start_row,
                selection.start_col,
            )
        };

    if row < start_row || row > end_row {
        return false;
    }

    if start_row == end_row {
        return row == start_row && col >= start_col && col <= end_col;
    }

    if row == start_row {
        return col >= start_col;
    }

    if row == end_row {
        return col <= end_col;
    }

    true
}

fn push_style_diff(output: &mut Vec<u8>, previous: CellStyle, next: CellStyle) {
    if previous == next {
        return;
    }

    output.extend_from_slice(b"\x1b[0m");

    let mut codes = Vec::new();
    if next.bold {
        codes.push("1".to_string());
    }
    if next.dim {
        codes.push("2".to_string());
    }
    if next.italic {
        codes.push("3".to_string());
    }
    if next.underline {
        codes.push("4".to_string());
    }
    if next.inverse {
        codes.push("7".to_string());
    }

    push_color_code(&mut codes, next.fg, true);
    push_color_code(&mut codes, next.bg, false);

    if codes.is_empty() {
        return;
    }

    output.extend_from_slice(b"\x1b[");
    output.extend_from_slice(codes.join(";").as_bytes());
    output.push(b'm');
}

fn push_color_code(codes: &mut Vec<String>, color: Color, foreground: bool) {
    match color {
        Color::Default => {
            codes.push(if foreground {
                "39".to_string()
            } else {
                "49".to_string()
            });
        }
        Color::Idx(value) => {
            let base = if foreground { 30_u8 } else { 40_u8 };
            let bright_base = if foreground { 90_u8 } else { 100_u8 };

            if value <= 7 {
                codes.push((base + value).to_string());
            } else if value <= 15 {
                codes.push((bright_base + (value - 8)).to_string());
            } else {
                if foreground {
                    codes.push("38".to_string());
                } else {
                    codes.push("48".to_string());
                }
                codes.push("5".to_string());
                codes.push(value.to_string());
            }
        }
        Color::Rgb(red, green, blue) => {
            if foreground {
                codes.push("38".to_string());
            } else {
                codes.push("48".to_string());
            }
            codes.push("2".to_string());
            codes.push(red.to_string());
            codes.push(green.to_string());
            codes.push(blue.to_string());
        }
    }
}

fn draw_changed_lines(
    stdout: &mut io::Stdout,
    pane: &PaneRenderData,
    previous: &[Vec<u8>],
    force: bool,
) -> Result<usize> {
    let inner = pane.rect.inner();
    if inner.width == 0 || inner.height == 0 {
        return Ok(0);
    }

    let mut changed = 0_usize;

    for (row_index, line) in pane.lines.iter().enumerate() {
        let line_changed = force || previous.get(row_index) != Some(line);
        if !line_changed {
            continue;
        }

        let y = inner
            .y
            .saturating_add(u16::try_from(row_index).unwrap_or(0));
        write_at_bytes(stdout, inner.x, y, line)?;
        changed += 1;
    }

    Ok(changed)
}

fn draw_rect_border(stdout: &mut io::Stdout, rect: Rect, color: &str, title: &str) -> Result<()> {
    if rect.width < 2 || rect.height < 2 {
        return Ok(());
    }

    let inner_width = usize::from(rect.width.saturating_sub(2));
    let mut title_inner = fit_to_width(title, inner_width);
    let title_w = UnicodeWidthStr::width(title_inner.as_str());
    if title_w < inner_width {
        title_inner.push_str(&"-".repeat(inner_width - title_w));
    }
    let top = format!("+{title_inner}+");
    let bottom = top.clone();
    let right_x = rect.x.saturating_add(rect.width.saturating_sub(1));

    write_at(stdout, rect.x, rect.y, &format!("{color}{top}\x1b[0m"))?;
    for offset in 1..rect.height.saturating_sub(1) {
        let y = rect.y.saturating_add(offset);
        write_at(stdout, rect.x, y, &format!("{color}|\x1b[0m"))?;
        write_at(stdout, right_x, y, &format!("{color}|\x1b[0m"))?;
    }
    write_at(
        stdout,
        rect.x,
        rect.y.saturating_add(rect.height.saturating_sub(1)),
        &format!("{color}{bottom}\x1b[0m"),
    )?;

    Ok(())
}

fn clear_body_region(stdout: &mut io::Stdout, cols: u16, rows: u16) -> Result<()> {
    if rows <= 1 || cols == 0 {
        return Ok(());
    }

    let blank = " ".repeat(usize::from(cols));
    for row in 2..=rows {
        write_at(stdout, 1, row, &blank)?;
    }

    Ok(())
}

fn write_at(stdout: &mut io::Stdout, x: u16, y: u16, text: &str) -> Result<()> {
    write!(stdout, "\x1b[{y};{x}H{text}").context("failed writing terminal content")
}

fn write_at_bytes(stdout: &mut io::Stdout, x: u16, y: u16, bytes: &[u8]) -> Result<()> {
    write!(stdout, "\x1b[{y};{x}H").context("failed moving cursor for bytes")?;
    stdout
        .write_all(bytes)
        .context("failed writing terminal byte content")?;
    write!(stdout, "\x1b[0m").context("failed resetting terminal attributes")
}

fn pad_or_truncate(text: &str, width: usize) -> String {
    let mut rendered = fit_to_width(text, width);
    let current_width = UnicodeWidthStr::width(rendered.as_str());
    if current_width < width {
        rendered.push_str(&" ".repeat(width.saturating_sub(current_width)));
    }

    rendered
}

fn fit_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let mut rendered = String::new();
    let mut used = 0_usize;
    for character in text.chars() {
        let char_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if char_width == 0 {
            continue;
        }

        if used + char_width > width {
            break;
        }

        rendered.push(character);
        used += char_width;
    }

    rendered
}

#[cfg(test)]
mod tests {
    use super::{CursorOverlay, pad_or_truncate, render_screen_row, terminal_cursor_control};
    use crate::pane::PaneId;
    use unicode_width::UnicodeWidthStr;
    use vt100::Parser;

    #[test]
    fn pad_or_truncate_handles_wide_characters() {
        let rendered = pad_or_truncate("a界b", 4);
        assert_eq!(UnicodeWidthStr::width(rendered.as_str()), 4);
    }

    #[test]
    fn row_renderer_keeps_colors_without_erase_controls() {
        let mut parser = Parser::new(2, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m text\x1b[K");

        let row = render_screen_row(parser.screen(), 0, 20, None, None);
        let rendered = String::from_utf8(row).expect("valid utf8 row output");

        assert!(rendered.contains("red"));
        assert!(rendered.contains("\x1b["));
        assert!(!rendered.contains("\x1b[K"));
        assert!(!rendered.contains("\x1b[2K"));
    }

    #[test]
    fn row_renderer_draws_visible_cursor_overlay() {
        let mut parser = Parser::new(2, 20, 0);
        parser.process(b"hello world");

        let row = render_screen_row(
            parser.screen(),
            0,
            20,
            Some(CursorOverlay {
                pane_id: PaneId(1),
                row: 0,
                col: 0,
            }),
            None,
        );
        let rendered = String::from_utf8(row).expect("valid utf8 row output");

        assert!(rendered.contains("\x1b["));
        assert!(rendered.contains("4;"));
        assert!(rendered.contains("7;"));
    }

    #[test]
    fn terminal_cursor_control_omits_show_sequence_when_hidden() {
        let control = terminal_cursor_control(false, 4, 9);
        assert!(control.is_none());
    }

    #[test]
    fn terminal_cursor_control_includes_show_sequence_when_enabled() {
        let control = terminal_cursor_control(true, 4, 9).expect("cursor control should exist");
        assert!(control.contains("\x1b[?25h"));
        assert!(control.contains("\x1b[4;9H"));
    }
}
