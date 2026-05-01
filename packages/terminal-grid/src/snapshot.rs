use crate::model::{Cursor, GridMode, PhysicalRow, TerminalGrid};
use crate::style::{Style, StyleId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRunSnapshot {
    pub start_col: u16,
    pub text: String,
    pub style: StyleId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowSnapshot {
    pub wrapped: bool,
    pub runs: Vec<CellRunSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorSnapshot {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub revision: u64,
    pub width: u16,
    pub height: u16,
    pub mode: String,
    pub scrollback_rows: u32,
    pub cursor: CursorSnapshot,
    pub styles: Vec<Style>,
    pub rows: Vec<RowSnapshot>,
}

impl GridSnapshot {
    #[must_use]
    pub fn from_grid(grid: &TerminalGrid, scrollback_offset: usize, rows: usize) -> Self {
        let all_rows = match grid.mode() {
            GridMode::Main => grid.all_main_rows(),
            GridMode::Alternate => grid.viewport_rows(),
        };
        let viewport_start = all_rows.len().saturating_sub(grid.height());
        let scrollback_rows = u32::try_from(viewport_start).unwrap_or(u32::MAX);
        let end = all_rows
            .len()
            .saturating_sub(scrollback_offset.min(all_rows.len()));
        let start = end.saturating_sub(rows.max(grid.height()));
        let selected_rows = all_rows[start..end]
            .iter()
            .map(|row| row_snapshot(row, grid.width()))
            .collect();
        let cursor = cursor_snapshot(grid.cursor());
        Self {
            revision: grid.revision(),
            width: u16::try_from(grid.width()).unwrap_or(u16::MAX),
            height: u16::try_from(grid.height()).unwrap_or(u16::MAX),
            mode: match grid.mode() {
                GridMode::Main => "main".to_string(),
                GridMode::Alternate => "alternate".to_string(),
            },
            scrollback_rows,
            cursor,
            styles: grid.palette().styles().to_vec(),
            rows: selected_rows,
        }
    }
}

fn cursor_snapshot(cursor: Cursor) -> CursorSnapshot {
    CursorSnapshot {
        row: u16::try_from(cursor.row).unwrap_or(u16::MAX),
        col: u16::try_from(cursor.col).unwrap_or(u16::MAX),
        visible: cursor.visible,
    }
}

fn row_snapshot(row: &PhysicalRow, width: usize) -> RowSnapshot {
    let cells = row.visual_cells(width);
    let mut runs = Vec::new();
    let mut current_start = 0_usize;
    let mut current_style = None::<StyleId>;
    let mut current_text = String::new();

    for (index, cell) in cells.iter().enumerate() {
        if cell.is_wide_continuation() {
            continue;
        }
        if cell.text() == " " && current_text.is_empty() {
            continue;
        }
        if current_style == Some(cell.style()) {
            current_text.push_str(cell.text());
            continue;
        }
        flush_run(&mut runs, current_start, current_style, &mut current_text);
        current_start = index;
        current_style = Some(cell.style());
        current_text.push_str(cell.text());
    }
    flush_run(&mut runs, current_start, current_style, &mut current_text);

    RowSnapshot {
        wrapped: row.wrapped(),
        runs,
    }
}

fn flush_run(
    runs: &mut Vec<CellRunSnapshot>,
    start: usize,
    style: Option<StyleId>,
    text: &mut String,
) {
    if text.is_empty() {
        return;
    }
    runs.push(CellRunSnapshot {
        start_col: u16::try_from(start).unwrap_or(u16::MAX),
        text: std::mem::take(text),
        style: style.unwrap_or(StyleId::DEFAULT),
    });
}

#[cfg(test)]
mod tests {
    use crate::model::{GridLimits, TerminalGrid};

    #[test]
    fn snapshot_encodes_style_runs() {
        let mut grid = TerminalGrid::new(20, 2, GridLimits::default()).unwrap();
        grid.process(b"plain \x1b[31mred");
        let snapshot = grid.snapshot(0, 2);
        assert_eq!(snapshot.rows.len(), 2);
        assert!(snapshot.rows[0].runs.len() >= 2);
    }
}
