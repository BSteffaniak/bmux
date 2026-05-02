use crate::{Color, PhysicalRow, Style, TerminalGrid};

/// Extract text from a physical row, preserving cell columns and skipping wide
/// continuation cells.
#[must_use]
pub fn row_text(row: &PhysicalRow, width: usize) -> String {
    let mut text = String::new();
    for col in 0..width {
        let Some(cell) = row.cells().get(col) else {
            text.push(' ');
            continue;
        };
        if !cell.is_wide_continuation() {
            text.push_str(cell.text());
        }
    }
    text
}

/// Extract visible text lines from a grid display slice.
#[must_use]
pub fn visible_text_lines(
    grid: &TerminalGrid,
    scrollback_offset: usize,
    rows: usize,
) -> Vec<String> {
    grid.display_rows(scrollback_offset, rows)
        .into_iter()
        .map(|row| row_text(&row, grid.width()))
        .collect()
}

/// Extract visible text from a grid display slice.
#[must_use]
pub fn visible_text(grid: &TerminalGrid, scrollback_offset: usize, rows: usize) -> String {
    visible_text_lines(grid, scrollback_offset, rows).join("\n")
}

/// Extract visible text and remove trailing blank lines.
#[must_use]
pub fn visible_text_trimmed(grid: &TerminalGrid, scrollback_offset: usize, rows: usize) -> String {
    let mut lines = visible_text_lines(grid, scrollback_offset, rows);
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Generate a full-screen ANSI repaint for the current viewport.
///
/// The repaint clears the screen, positions each viewport row explicitly, emits
/// style changes as SGR sequences, writes every cell column, and resets style at
/// the end. This is intended for replay/export baselines rather than incremental
/// terminal updates.
#[must_use]
pub fn full_screen_repaint_bytes(grid: &TerminalGrid) -> Vec<u8> {
    let rows = grid.viewport_rows();
    if rows.is_empty() {
        return Vec::new();
    }

    let mut bytes = b"\x1b[0m\x1b[2J".to_vec();
    for (row_index, row) in rows.iter().enumerate() {
        bytes.extend_from_slice(format!("\x1b[{};1H", row_index.saturating_add(1)).as_bytes());
        let mut current_style = Style::default();
        for col in 0..grid.width() {
            let cell = row.cells().get(col);
            let style = cell
                .map(|cell| grid.palette().get(cell.style()))
                .unwrap_or_default();
            if style != current_style {
                bytes.extend_from_slice(style_sgr(style).as_bytes());
                current_style = style;
            }
            if let Some(cell) = cell
                && !cell.is_wide_continuation()
                && !cell.text().is_empty()
            {
                bytes.extend_from_slice(cell.text().as_bytes());
            } else {
                bytes.push(b' ');
            }
        }
    }
    bytes.extend_from_slice(b"\x1b[0m");
    bytes
}

fn style_sgr(style: Style) -> String {
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
    if style.strike {
        parts.push("9".to_string());
    }
    push_color_sgr(&mut parts, style.fg, true);
    push_color_sgr(&mut parts, style.bg, false);
    format!("\x1b[{}m", parts.join(";"))
}

fn push_color_sgr(parts: &mut Vec<String>, color: Option<Color>, foreground: bool) {
    match color {
        None => parts.push(if foreground { "39" } else { "49" }.to_string()),
        Some(Color::Indexed(index)) => {
            parts.push(if foreground { "38" } else { "48" }.to_string());
            parts.push("5".to_string());
            parts.push(index.to_string());
        }
        Some(Color::Rgb { r, g, b }) => {
            parts.push(if foreground { "38" } else { "48" }.to_string());
            parts.push("2".to_string());
            parts.push(r.to_string());
            parts.push(g.to_string());
            parts.push(b.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{GridLimits, TerminalGridStream};

    use super::{full_screen_repaint_bytes, visible_text, visible_text_trimmed};

    fn test_grid(cols: u16, rows: u16) -> TerminalGridStream {
        TerminalGridStream::new(cols, rows, GridLimits::default())
            .expect("test grid dimensions are valid")
    }

    #[test]
    fn visible_text_preserves_columns_and_trimmed_drops_blank_trailing_rows() {
        let mut grid = test_grid(8, 3);
        grid.process(b"hi\r\nthere");

        assert_eq!(
            visible_text(grid.grid(), 0, 3),
            "hi      \nthere   \n        "
        );
        assert_eq!(
            visible_text_trimmed(grid.grid(), 0, 3),
            "hi      \nthere   "
        );
    }

    #[test]
    fn repaint_reconstructs_visible_text() {
        let mut grid = test_grid(80, 24);
        grid.process(b"hello\r\nworld");

        let repaint = full_screen_repaint_bytes(grid.grid());
        let mut replay = test_grid(80, 24);
        replay.process(&repaint);

        let contents = visible_text(replay.grid(), 0, replay.grid().height());
        assert!(contents.contains("hello"));
        assert!(contents.contains("world"));
    }

    #[test]
    fn repaint_preserves_sgr_styles() {
        let mut grid = test_grid(16, 2);
        grid.process(b"\x1b[1;3;4;9;38;2;1;2;3;48;5;4mA");

        let repaint = full_screen_repaint_bytes(grid.grid());
        let repaint_text = String::from_utf8(repaint).expect("repaint is utf8 ansi text");

        assert!(repaint_text.contains("\x1b[1;1H"));
        assert!(repaint_text.contains("1;3;4;9;38;2;1;2;3;48;5;4"));
        assert!(repaint_text.contains('A'));
    }

    #[test]
    fn visible_text_skips_wide_continuation_cells() {
        let mut grid = test_grid(4, 1);
        grid.process("好b".as_bytes());

        assert_eq!(visible_text_trimmed(grid.grid(), 0, 1), "好b ");
    }
}
