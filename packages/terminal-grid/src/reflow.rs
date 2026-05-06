use crate::model::{Cell, PhysicalRow};
use std::collections::VecDeque;

pub(crate) fn project_logical_line(cells: &[Cell], width: usize) -> VecDeque<PhysicalRow> {
    let mut rows = VecDeque::new();
    push_reflowed_logical_line(&mut rows, cells, width);
    rows
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
        if cell.text() == " " && !cell.is_wide_continuation() {
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
