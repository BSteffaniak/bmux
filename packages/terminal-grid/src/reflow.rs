use crate::model::{Cell, PhysicalRow};
use std::collections::VecDeque;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
static PROJECTED_LOGICAL_LINES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static PROJECTED_PHYSICAL_ROWS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProjectionStats {
    pub logical_lines_projected: usize,
    pub physical_rows_projected: usize,
}

#[cfg(test)]
pub(crate) fn reset_projection_stats() {
    PROJECTED_LOGICAL_LINES.store(0, Ordering::Relaxed);
    PROJECTED_PHYSICAL_ROWS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn projection_stats() -> ProjectionStats {
    ProjectionStats {
        logical_lines_projected: PROJECTED_LOGICAL_LINES.load(Ordering::Relaxed),
        physical_rows_projected: PROJECTED_PHYSICAL_ROWS.load(Ordering::Relaxed),
    }
}

pub(crate) fn project_logical_line(cells: &[Cell], width: usize) -> VecDeque<PhysicalRow> {
    #[cfg(test)]
    PROJECTED_LOGICAL_LINES.fetch_add(1, Ordering::Relaxed);

    let mut rows = VecDeque::new();
    push_reflowed_logical_line(&mut rows, cells, width);

    #[cfg(test)]
    PROJECTED_PHYSICAL_ROWS.fetch_add(rows.len(), Ordering::Relaxed);

    rows
}

pub(crate) fn projected_logical_line_row_count(cells: &[Cell], width: usize) -> usize {
    let width = width.max(1);
    let cells = trim_trailing_blank_cells(cells);
    if cells.is_empty() {
        return 1;
    }

    let mut rows = 0_usize;
    let mut col = 0_usize;
    let mut current_has_cells = false;
    for cell in cells {
        let cell_width = usize::from(cell.width()).max(1);
        if col > 0 && col.saturating_add(cell_width) > width {
            rows = rows.saturating_add(1);
            col = 0;
        }
        col = col.saturating_add(cell_width).min(width);
        current_has_cells = true;
        if col >= width {
            rows = rows.saturating_add(1);
            col = 0;
            current_has_cells = false;
        }
    }
    if current_has_cells {
        rows = rows.saturating_add(1);
    }
    rows.max(1)
}

fn push_reflowed_logical_line(rows: &mut VecDeque<PhysicalRow>, cells: &[Cell], width: usize) {
    if cells.is_empty() {
        rows.push_back(PhysicalRow::new());
        return;
    }

    let mut current = PhysicalRow::new();
    let mut col = 0_usize;
    let mut emitted_any = false;

    for cell in trim_trailing_blank_cells(cells) {
        let cell_width = usize::from(cell.width()).max(1);
        if col > 0 && col + cell_width > width {
            current.set_wrapped(true);
            rows.push_back(current);
            current = PhysicalRow::new();
            col = 0;
        }

        current.set_cell(col, cell.clone());
        if cell_width == 2 && col + 1 < width {
            current.set_cell(col + 1, Cell::spacer(cell.style()));
        }
        col = col.saturating_add(cell_width).min(width);
        emitted_any = true;

        if col >= width {
            current.set_wrapped(true);
            rows.push_back(current);
            current = PhysicalRow::new();
            col = 0;
        }
    }

    if !emitted_any || !current.cells().is_empty() {
        current.set_wrapped(false);
        rows.push_back(current);
    } else if let Some(last) = rows.back_mut() {
        last.set_wrapped(false);
    }
}

fn trim_trailing_blank_cells(cells: &[Cell]) -> &[Cell] {
    let mut end = cells.len();
    while end > 0 {
        let cell = &cells[end - 1];
        if cell.is_discardable_blank() {
            end -= 1;
        } else {
            break;
        }
    }
    &cells[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::StyleId;

    #[test]
    fn projects_logical_cells_to_requested_width() {
        let cells = row("abcdef").visual_cells(6);
        let rows = project_logical_line(&cells, 3);

        assert_eq!(text(&rows[0]), "abc");
        assert!(rows[0].wrapped());
        assert_eq!(text(&rows[1]), "def");
        assert!(!rows[1].wrapped());
    }

    fn row(text: &str) -> PhysicalRow {
        let mut row = PhysicalRow::new();
        for (index, ch) in text.chars().enumerate() {
            row.set_cell(index, Cell::new(ch.to_string(), StyleId::DEFAULT, 1));
        }
        row
    }

    fn text(row: &PhysicalRow) -> String {
        row.cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(Cell::text)
            .collect()
    }
}
