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
    projected_logical_line_row_count_retained(cells, width, false)
}

pub(crate) fn projected_logical_line_row_count_retained(
    cells: &[Cell],
    width: usize,
    retain: bool,
) -> usize {
    let width = width.max(1);
    let cells = projection_cells(cells, retain);
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
    admit_logical_text_retained(cells, width, range, remaining, false)
}

pub(crate) fn admit_logical_text_retained(
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
    remaining: &mut usize,
    retain: bool,
) -> Option<()> {
    let width = width.max(1);
    let mut row = 0_usize;
    let mut col = 0_usize;
    for cell in projection_cells(cells, retain) {
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
    projected_cell_storage_retained(cells, width, range, false)
}

pub(crate) fn projected_cell_storage_retained(
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
    retain: bool,
) -> Option<usize> {
    let width = width.max(1);
    let mut row = 0_usize;
    let mut col = 0_usize;
    let mut extent = 0_usize;
    let mut total = 0_usize;
    let mut largest = 0_usize;
    for cell in projection_cells(cells, retain) {
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
    try_project_logical_line_window_retained(cells, width, range, false)
}

pub(crate) fn try_project_logical_line_window_retained(
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
    retain: bool,
) -> Option<VecDeque<PhysicalRow>> {
    #[cfg(test)]
    PROJECTED_LOGICAL_LINES.set(PROJECTED_LOGICAL_LINES.get() + 1);

    let mut rows = VecDeque::new();
    rows.try_reserve_exact(
        range
            .end
            .min(projected_logical_line_row_count_retained(
                cells, width, retain,
            ))
            .saturating_sub(range.start),
    )
    .ok()?;
    push_reflowed_logical_line(&mut rows, cells, width, range, retain)?;

    #[cfg(test)]
    PROJECTED_PHYSICAL_ROWS.set(PROJECTED_PHYSICAL_ROWS.get() + rows.len());

    Some(rows)
}

/// Visit row boundaries without copying cells. The same wide-cell and
/// one-column overflow rules drive both indexed and ordinary projection.
pub(crate) fn visit_logical_rows<E>(
    cells: &[Cell],
    width: usize,
    mut visit: impl FnMut(std::ops::Range<usize>, usize, bool) -> Result<(), E>,
) -> Result<(), E> {
    let width = width.max(1);
    let mut start = 0;
    let mut col = 0;
    let mut logical = 0;
    let mut row_column = 0;
    for (index, cell) in cells.iter().enumerate() {
        let size = usize::from(cell.width()).max(1);
        if col > 0 && col + size > width {
            visit(start..index, row_column, true)?;
            start = index;
            row_column = logical;
            col = 0;
        }
        col = (col + size).min(width);
        logical += size;
        if col == width {
            visit(start..index + 1, row_column, index + 1 < cells.len())?;
            start = index + 1;
            row_column = logical;
            col = 0;
        }
    }
    if start < cells.len() || cells.is_empty() {
        visit(start..cells.len(), row_column, false)?;
    }
    Ok(())
}

fn push_reflowed_logical_line(
    rows: &mut VecDeque<PhysicalRow>,
    cells: &[Cell],
    width: usize,
    range: std::ops::Range<usize>,
    retain: bool,
) -> Option<()> {
    let width = width.max(1);
    if range.is_empty() {
        return Some(());
    }
    let cells = projection_cells(cells, retain);
    let mut index = 0;
    // The visitor is shared with the caller-owned width index. Stop traversal
    // once the requested range has been emitted.
    let result = visit_logical_rows(cells, width, |selected, _, wrapped| {
        if index >= range.end {
            return Err(false);
        }
        if range.contains(&index) {
            let mut row = PhysicalRow::new();
            let mut col = 0;
            for cell in &cells[selected] {
                row.try_set_projected_cell(col, cell, width).ok_or(true)?;
                col = (col + usize::from(cell.width()).max(1)).min(width);
            }
            row.set_wrapped(wrapped);
            rows.push_back(row);
        }
        index += 1;
        Ok(())
    });
    if result == Err(true) { None } else { Some(()) }
}

/// Logical column at the start of a projected row. Uses the same trimming,
/// wide-cell overflow and one-column clipping rules as physical projection.
/// Empty lines map row zero to column zero; their exclusive row bound is
/// intentionally unavailable because a bare column cannot distinguish both.
/// Nonempty lines map the exclusive row bound to the end of visible cells.
#[cfg(test)]
pub(crate) fn logical_column_for_row(cells: &[Cell], width: usize, target: usize) -> Option<usize> {
    logical_column_for_row_retained(cells, width, target, false)
}

pub(crate) fn logical_column_for_row_retained(
    cells: &[Cell],
    width: usize,
    target: usize,
    retain: bool,
) -> Option<usize> {
    let cells = projection_cells(cells, retain);
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
    let count = projected_logical_line_row_count_retained(cells, width, retain);
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
#[cfg(test)]
pub(crate) fn row_for_logical_column(cells: &[Cell], width: usize, target: usize) -> Option<usize> {
    row_for_logical_column_retained(cells, width, target, false)
}

pub(crate) fn row_for_logical_column_retained(
    cells: &[Cell],
    width: usize,
    target: usize,
    retain: bool,
) -> Option<usize> {
    let cells = projection_cells(cells, retain);
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
    (logical == target).then(|| projected_logical_line_row_count_retained(cells, width, retain))
}

fn projection_cells(cells: &[Cell], retain: bool) -> &[Cell] {
    if retain {
        return cells;
    }
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
