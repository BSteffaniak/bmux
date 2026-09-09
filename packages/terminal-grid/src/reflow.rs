use crate::model::{Cell, PhysicalRow};
use std::collections::VecDeque;

#[cfg(test)]
std::thread_local! {
    static PROJECTED_LOGICAL_LINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PROJECTED_PHYSICAL_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProjectionStats {
    pub logical_lines_projected: usize,
    pub physical_rows_projected: usize,
}

#[cfg(test)]
pub(crate) fn reset_projection_stats() {
    PROJECTED_LOGICAL_LINES.set(0);
    PROJECTED_PHYSICAL_ROWS.set(0);
}

#[cfg(test)]
pub(crate) fn projection_stats() -> ProjectionStats {
    ProjectionStats {
        logical_lines_projected: PROJECTED_LOGICAL_LINES.get(),
        physical_rows_projected: PROJECTED_PHYSICAL_ROWS.get(),
    }
}

pub(crate) fn project_logical_line(cells: &[Cell], width: usize) -> VecDeque<PhysicalRow> {
    project_logical_line_window(cells, width, 0..usize::MAX)
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

/// Charge selected logical cells before projection can clone their text.
pub(crate) fn admit_logical_text(
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
    remaining: &mut usize,
) -> Option<()> {
    let width = width.max(1);
    let mut row = 0_usize;
    let mut col = 0_usize;
    for cell in trim_trailing_blank_cells(cells) {
        let cell_width = usize::from(cell.width()).max(1);
        if col > 0 && col.saturating_add(cell_width) > width {
            row = row.saturating_add(1);
            col = 0;
        }
        if row >= range.end {
            break;
        }
        if range.contains(&row) {
            *remaining = remaining.checked_sub(cell.text().len())?;
        }
        col = col.saturating_add(cell_width).min(width);
        if col >= width {
            row = row.saturating_add(1);
            col = 0;
        }
    }
    Some(())
}

/// Requested cell storage for a selected projection, without allocating rows.
/// Includes implicit gaps/spacers and one replacement buffer for the widest
/// selected row. Allocator excess is accounted separately after projection.
pub(crate) fn projected_cell_storage(
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
) -> Option<usize> {
    let width = width.max(1);
    let mut row = 0_usize;
    let mut col = 0_usize;
    let mut extent = 0_usize;
    let mut total = 0_usize;
    let mut largest = 0_usize;
    for cell in trim_trailing_blank_cells(cells) {
        let cell_width = usize::from(cell.width()).max(1);
        if col > 0 && col.saturating_add(cell_width) > width {
            total = total.checked_add(extent)?;
            largest = largest.max(extent);
            extent = 0;
            row = row.saturating_add(1);
            col = 0;
        }
        if row >= range.end {
            break;
        }
        if range.contains(&row) {
            extent = col.checked_add(if cell_width == 2 && col + 1 < width {
                2
            } else {
                1
            })?;
        }
        col = col.saturating_add(cell_width).min(width);
        if col >= width {
            total = total.checked_add(extent)?;
            largest = largest.max(extent);
            extent = 0;
            row = row.saturating_add(1);
            col = 0;
        }
    }
    total = total.checked_add(extent)?;
    largest = largest.max(extent);
    // Each slot can temporarily contain a one-byte implicit blank.
    total
        .checked_mul(std::mem::size_of::<Cell>() + 1)?
        .checked_add(largest.checked_mul(std::mem::size_of::<Cell>())?)
}

pub(crate) fn project_logical_line_window(
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
) -> VecDeque<PhysicalRow> {
    try_project_logical_line_window(cells, width, range).expect("projection allocation failed")
}

pub(crate) fn try_project_logical_line_window(
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
) -> Option<VecDeque<PhysicalRow>> {
    #[cfg(test)]
    PROJECTED_LOGICAL_LINES.set(PROJECTED_LOGICAL_LINES.get() + 1);

    let mut rows = VecDeque::new();
    rows.try_reserve_exact(
        range
            .end
            .min(projected_logical_line_row_count(cells, width))
            .saturating_sub(range.start),
    )
    .ok()?;
    push_reflowed_logical_line(&mut rows, cells, width, range)?;

    #[cfg(test)]
    PROJECTED_PHYSICAL_ROWS.set(PROJECTED_PHYSICAL_ROWS.get() + rows.len());

    Some(rows)
}

fn push_reflowed_logical_line(
    rows: &mut VecDeque<PhysicalRow>,
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
) -> Option<()> {
    let width = width.max(1);
    if range.is_empty() {
        return Some(());
    }
    if cells.is_empty() {
        if range.contains(&0) {
            rows.push_back(PhysicalRow::new());
        }
        return Some(());
    }

    let mut index = 0;
    let mut current = PhysicalRow::new();
    let mut col = 0_usize;
    let mut emitted_any = false;

    for cell in trim_trailing_blank_cells(cells) {
        // Seeing another cell proves the last emitted row is a continuation.
        // If the line ended exactly there, the finalization below instead
        // clears its wrap flag.
        if index >= range.end {
            return Some(());
        }
        let cell_width = usize::from(cell.width()).max(1);
        if col > 0 && col + cell_width > width {
            current.set_wrapped(true);
            if range.contains(&index) {
                rows.push_back(current);
            }
            index += 1;
            current = PhysicalRow::new();
            col = 0;
        }

        if range.contains(&index) {
            current.try_set_projected_cell(col, cell, width)?;
        }
        col = col.saturating_add(cell_width).min(width);
        emitted_any = true;

        if col >= width {
            current.set_wrapped(true);
            if range.contains(&index) {
                rows.push_back(current);
            }
            index += 1;
            current = PhysicalRow::new();
            col = 0;
        }
    }

    if !emitted_any || col > 0 {
        if range.contains(&index) {
            current.set_wrapped(false);
            rows.push_back(current);
        }
    } else if range.contains(&index.saturating_sub(1))
        && let Some(last) = rows.back_mut()
    {
        last.set_wrapped(false);
    }
    Some(())
}

/// Logical column at the start of a projected row. Uses the same trimming,
/// wide-cell overflow and one-column clipping rules as physical projection.
/// Empty lines map row zero to column zero; their exclusive row bound is
/// intentionally unavailable because a bare column cannot distinguish both.
/// Nonempty lines map the exclusive row bound to the end of visible cells.
pub(crate) fn logical_column_for_row(cells: &[Cell], width: usize, target: usize) -> Option<usize> {
    let cells = trim_trailing_blank_cells(cells);
    if cells.is_empty() {
        return (target == 0).then_some(0);
    }
    let width = width.max(1);
    let mut row = 0;
    let mut col = 0;
    let mut logical = 0_usize;
    for cell in cells {
        let extent = usize::from(cell.width()).max(1);
        if col > 0 && col + extent > width {
            row += 1;
            col = 0;
        }
        if row == target && col == 0 {
            return Some(logical);
        }
        logical = logical.checked_add(extent)?;
        col = (col + extent).min(width);
        if col == width {
            row += 1;
            col = 0;
        }
    }
    let count = projected_logical_line_row_count(cells, width);
    if target == count {
        Some(logical)
    } else if target == 0 {
        Some(0)
    } else {
        None
    }
}

/// Project a cell-boundary anchor into a row, rejecting offsets inside wide
/// cells and trimmed trailing padding. End-of-line maps to the exclusive row
/// bound rather than inventing a blank continuation row. For an empty or
/// entirely trimmed line, column zero instead identifies the visible empty row.
pub(crate) fn row_for_logical_column(cells: &[Cell], width: usize, target: usize) -> Option<usize> {
    let cells = trim_trailing_blank_cells(cells);
    if cells.is_empty() {
        return (target == 0).then_some(0);
    }
    let width = width.max(1);
    let mut row = 0;
    let mut col = 0;
    let mut logical = 0_usize;
    for cell in cells {
        let extent = usize::from(cell.width()).max(1);
        if col > 0 && col + extent > width {
            row += 1;
            col = 0;
        }
        if logical == target {
            return Some(row);
        }
        logical = logical.checked_add(extent)?;
        if logical > target {
            return None;
        }
        col = (col + extent).min(width);
        if col == width {
            row += 1;
            col = 0;
        }
    }
    (logical == target).then(|| projected_logical_line_row_count(cells, width))
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
    fn empty_line_anchor_is_the_visible_row_not_exclusive_end() {
        for cells in [Vec::new(), vec![Cell::new(" ", StyleId::DEFAULT, 1)]] {
            for width in 1..=5 {
                assert_eq!(logical_column_for_row(&cells, width, 0), Some(0));
                assert_eq!(row_for_logical_column(&cells, width, 0), Some(0));
                assert_eq!(logical_column_for_row(&cells, width, 1), None);
                assert_eq!(row_for_logical_column(&cells, width, 1), None);
            }
        }
    }

    #[test]
    fn row_anchors_follow_wide_cell_overflow_and_round_trip() {
        let cells = vec![
            Cell::new("a", StyleId::DEFAULT, 1),
            Cell::new("界", StyleId::DEFAULT, 2),
            Cell::new("b", StyleId::DEFAULT, 1),
        ];
        assert_eq!(logical_column_for_row(&cells, 2, 1), Some(1));
        assert_eq!(row_for_logical_column(&cells, 3, 1), Some(0));
        assert_eq!(row_for_logical_column(&cells, 2, 2), None);
        for width in 1..=5 {
            let count = projected_logical_line_row_count(&cells, width);
            for row in 0..=count {
                let column = logical_column_for_row(&cells, width, row).unwrap();
                assert_eq!(row_for_logical_column(&cells, width, column), Some(row));
            }
        }
    }

    #[test]
    fn selected_storage_uses_cell_extents_not_terminal_width() {
        let cells = vec![
            Cell::new("界", StyleId::DEFAULT, 2),
            Cell::new("a\u{0301}", StyleId::DEFAULT, 1),
        ];
        let slot = std::mem::size_of::<Cell>();
        assert_eq!(
            projected_cell_storage(&cells, 1000, 0..1),
            Some(3 * (slot + 1) + 3 * slot)
        );
        assert_eq!(
            projected_cell_storage(&cells, 2, 1..2),
            Some(slot + 1 + slot)
        );
        assert_eq!(projected_cell_storage(&cells, 2, 2..3), Some(0));
        assert_eq!(
            projected_cell_storage(&cells, 1, 0..1),
            Some(slot + 1 + slot)
        );
    }

    #[test]
    fn projection_stats_are_isolated_between_threads() {
        reset_projection_stats();
        let cells = row("abcdef").visual_cells(6);
        project_logical_line_window(&cells, 2, 1..2);
        let expected = ProjectionStats {
            logical_lines_projected: 1,
            physical_rows_projected: 1,
        };
        assert_eq!(projection_stats(), expected);
        std::thread::spawn(move || {
            assert_eq!(projection_stats(), ProjectionStats::default());
            project_logical_line(&cells, 2);
            assert_eq!(projection_stats().physical_rows_projected, 3);
            reset_projection_stats();
        })
        .join()
        .unwrap();
        assert_eq!(projection_stats(), expected);
    }

    #[test]
    fn long_line_windows_materialize_only_requested_rows() {
        let cells = vec![Cell::new("x".to_owned(), StyleId::DEFAULT, 1); 100_000];
        for range in [0..2, 5_000..5_002, 9_998..10_000] {
            reset_projection_stats();
            let rows = project_logical_line_window(&cells, 10, range.clone());
            assert_eq!(rows.len(), 2);
            assert_eq!(projection_stats().physical_rows_projected, 2);
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(text(row), "xxxxxxxxxx");
                assert_eq!(row.wrapped(), range.start + index < 9_999);
            }
        }
    }

    #[test]
    fn projects_logical_cells_to_requested_width() {
        let cells = row("abcdef").visual_cells(6);
        let rows = project_logical_line(&cells, 3);

        assert_eq!(text(&rows[0]), "abc");
        assert!(rows[0].wrapped());
        assert_eq!(text(&rows[1]), "def");
        assert!(!rows[1].wrapped());
    }

    #[test]
    fn window_matches_full_projection_for_wide_and_blank_cells() {
        for input in ["", "abcdef", "ab界c界def", "abc   "] {
            let cells: Vec<_> = input
                .chars()
                .map(|ch| {
                    Cell::new(
                        ch.to_string(),
                        StyleId::DEFAULT,
                        if ch == '界' { 2 } else { 1 },
                    )
                })
                .collect();
            for width in 1..=7 {
                let full = project_logical_line(&cells, width);
                for start in 0..=full.len() {
                    for end in start..=full.len() {
                        let window = project_logical_line_window(&cells, width, start..end);
                        let expected: VecDeque<_> =
                            full.iter().skip(start).take(end - start).cloned().collect();
                        assert_eq!(
                            window, expected,
                            "input={input:?}, width={width}, range={start}..{end}"
                        );
                    }
                }
            }
        }
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
