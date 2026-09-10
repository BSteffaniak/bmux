//! Non-mutating, caller-owned transcript projection.

use super::{Cell, GridMode, PhysicalRow, TerminalGrid, physical_row_is_blank};
use crate::HistorySliceError;
use std::ops::Range;

#[cfg(test)]
mod tests;

/// Explicit work and retained-memory allowances for one content capture.
/// Exhaustion is reported, never silently interpreted as an empty terminal.
#[derive(Debug, Clone, Copy)]
pub struct ContentBudget {
    /// Maximum source cells/implicit columns inspected while capturing content.
    pub cells: usize,
    /// Maximum bytes retained for cells, text, and line metadata.
    pub bytes: usize,
}

/// A content position within a caller-owned capture. Valid across presentation
/// widths, but not across captures. Callers must change `capture` when replacing
/// the source or accepting a new content revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentAnchor {
    pub capture: u128,
    pub line: usize,
    pub column: usize,
}

/// A bounded projected window, with source positions for its row starts.
#[derive(Debug)]
pub struct ContentRows {
    pub rows: Vec<PhysicalRow>,
    pub anchors: Vec<ContentAnchor>,
    pub has_more_above: bool,
    pub has_more_below: bool,
    /// The source grid has already evicted history. No missing prefix is invented.
    pub history_truncated: bool,
}

#[derive(Debug)]
struct ContentLine {
    cells: Vec<Cell>,
    open: bool,
}

/// Reusable presentation capture, independent of terminal execution geometry.
///
/// Capture a bounded tail once per content revision, then project many widths
/// without copying the grid or replaying bytes. One width index is retained;
/// changing widths replaces it. Row anchors are stable within this capture.
/// Live edits and eviction require a new capture: old anchors fail closed.
#[derive(Debug)]
pub struct ContentProjection {
    capture: u128,
    revision: u64,
    lines: Vec<ContentLine>,
    more_above: bool,
    history_truncated: bool,
    index: Option<WidthIndex>,
}

#[derive(Debug)]
struct WidthIndex {
    width: usize,
    rows: Vec<IndexedRow>,
}

#[derive(Debug)]
struct IndexedRow {
    line: usize,
    cells: Range<usize>,
    column: usize,
    wrapped: bool,
}

impl ContentBudget {
    fn charge(&mut self, cells: usize, bytes: usize) -> Result<(), HistorySliceError> {
        self.cells = self
            .cells
            .checked_sub(cells)
            .ok_or(HistorySliceError::BudgetExhausted)?;
        self.bytes = self
            .bytes
            .checked_sub(bytes)
            .ok_or(HistorySliceError::BudgetExhausted)?;
        Ok(())
    }
}

impl TerminalGrid {
    /// Capture up to `max_lines` logical main-screen tail lines without changing
    /// terminal geometry. Main history remains accessible in alternate mode.
    ///
    /// Work and allocations are admitted before copying; even a single giant
    /// line can exhaust the budget. No full-history materialization occurs.
    /// `capture` must uniquely identify this source and revision for the caller.
    ///
    /// # Errors
    /// Returns `BudgetExhausted` if capture cannot fit the supplied allowances.
    pub fn capture_content(
        &self,
        capture: u128,
        max_lines: usize,
        mut budget: ContentBudget,
    ) -> Result<ContentProjection, HistorySliceError> {
        let mut lines = Vec::new();
        let mut end = self.main_rows.len();
        // Match transcript content semantics: unused trailing rectangle rows
        // are not content, including a blank cursor row following a newline.
        while end > 0 {
            let row = &self.main_rows[end - 1];
            budget.charge(row.cells().len().max(1), 0)?;
            if !physical_row_is_blank(row) {
                break;
            }
            end -= 1;
        }
        let mut pending_used = false;
        while end > 0 && lines.len() < max_lines {
            let mut start = end - 1;
            while start > 0 && self.main_rows[start - 1].wrapped() {
                budget.charge(1, 0)?;
                start -= 1;
            }
            let mut cells = Vec::new();
            if start == 0 && !self.pending_history_cells.is_empty() {
                copy_cells(&mut cells, &self.pending_history_cells, &mut budget)?;
                pending_used = true;
            }
            for row in self.main_rows.range(start..end) {
                // Wrapped physical rows preserve implicit padding to the capture
                // width; hard-ended rows need only their materialized extent.
                let columns = if row.wrapped() {
                    self.width
                } else {
                    row.cells().len()
                };
                budget.charge(columns.max(1), 0)?;
                for col in 0..columns {
                    if let Some(cell) = row.cells().get(col) {
                        if !cell.is_wide_continuation() {
                            copy_cell(&mut cells, cell, &mut budget)?;
                        }
                    } else {
                        copy_cell(
                            &mut cells,
                            &Cell::blank(crate::StyleId::DEFAULT),
                            &mut budget,
                        )?;
                    }
                }
            }
            push_line(
                &mut lines,
                cells,
                self.main_rows[end - 1].wrapped(),
                &mut budget,
            )?;
            end = start;
        }
        if end == 0
            && !pending_used
            && !self.pending_history_cells.is_empty()
            && lines.len() < max_lines
        {
            let mut cells = Vec::new();
            copy_cells(&mut cells, &self.pending_history_cells, &mut budget)?;
            push_line(&mut lines, cells, true, &mut budget)?;
            pending_used = true;
        }
        let mut history_used = 0;
        if end == 0 && (pending_used || self.pending_history_cells.is_empty()) {
            for line in self
                .main_history
                .iter()
                .rev()
                .take(max_lines.saturating_sub(lines.len()))
            {
                let mut cells = Vec::new();
                copy_cells(&mut cells, &line.cells, &mut budget)?;
                push_line(&mut lines, cells, false, &mut budget)?;
                history_used += 1;
            }
        }
        lines.reverse();
        Ok(ContentProjection {
            capture,
            revision: self.content_revision,
            lines,
            more_above: end > 0
                || (!pending_used && !self.pending_history_cells.is_empty())
                || history_used < self.main_history.len(),
            history_truncated: self.history_truncated,
            index: None,
        })
    }

    /// Crop the active positioned screen without reflow or terminal mutation.
    /// Horizontal offsets count physical columns. A clipped wide-cell fragment
    /// is left blank rather than emitted as a broken glyph.
    ///
    /// # Errors
    /// Returns `BudgetExhausted` when the selected cells/rows exceed the budget.
    pub fn screen_window(
        &self,
        columns: Range<usize>,
        rows: Range<usize>,
        mut budget: ContentBudget,
    ) -> Result<Vec<PhysicalRow>, HistorySliceError> {
        let width = columns.end.min(self.width).saturating_sub(columns.start);
        let mut output = Vec::new();
        for y in rows.start..rows.end.min(self.height) {
            let source = match self.mode {
                GridMode::Main => self.main_rows.get(y),
                GridMode::Alternate => self.alt_rows.get(y),
            };
            let Some(source) = source else {
                break;
            };
            budget.charge(width.max(1), std::mem::size_of::<PhysicalRow>())?;
            output
                .try_reserve_exact(1)
                .map_err(|_| HistorySliceError::BudgetExhausted)?;
            let mut row = PhysicalRow::new();
            for (x, cell) in source
                .cells()
                .iter()
                .enumerate()
                .skip(columns.start)
                .take(width)
            {
                if cell.is_wide_continuation()
                    || x.saturating_add(usize::from(cell.width())) > columns.end
                {
                    continue;
                }
                let gap = (x - columns.start).saturating_sub(row.cells().len());
                budget.charge(0, gap.saturating_mul(std::mem::size_of::<Cell>() + 1))?;
                charge_projected_cell(&mut budget.bytes, cell)?;
                row.try_set_projected_cell(x - columns.start, cell, width)
                    .ok_or(HistorySliceError::BudgetExhausted)?;
            }
            output.push(row);
        }
        Ok(output)
    }
}

// Charge geometric capacity growth before allocation rather than reallocating
// once per cell/line. Near a budget boundary, admit only the next item.
fn reserve_item<T>(items: &mut Vec<T>, bytes: &mut usize) -> Result<(), HistorySliceError> {
    if items.len() < items.capacity() {
        return Ok(());
    }
    let size = std::mem::size_of::<T>().max(1);
    let available = *bytes / size;
    let additional = items.capacity().max(1).min(available);
    if additional == 0 {
        return Err(HistorySliceError::BudgetExhausted);
    }
    *bytes -= additional * size;
    items
        .try_reserve_exact(additional)
        .map_err(|_| HistorySliceError::BudgetExhausted)
}

fn copy_cell(
    output: &mut Vec<Cell>,
    cell: &Cell,
    budget: &mut ContentBudget,
) -> Result<(), HistorySliceError> {
    budget.charge(0, cell.text().len())?;
    reserve_item(output, &mut budget.bytes)?;
    let mut text = String::new();
    text.try_reserve_exact(cell.text().len())
        .map_err(|_| HistorySliceError::BudgetExhausted)?;
    text.push_str(cell.text());
    output.push(Cell::new(text, cell.style(), cell.width()));
    Ok(())
}

fn copy_cells(
    output: &mut Vec<Cell>,
    cells: &[Cell],
    budget: &mut ContentBudget,
) -> Result<(), HistorySliceError> {
    budget.charge(cells.len().max(1), 0)?;
    for cell in cells {
        copy_cell(output, cell, budget)?;
    }
    Ok(())
}

fn push_line(
    lines: &mut Vec<ContentLine>,
    mut cells: Vec<Cell>,
    open: bool,
    budget: &mut ContentBudget,
) -> Result<(), HistorySliceError> {
    while !open && cells.last().is_some_and(Cell::is_discardable_blank) {
        cells.pop();
    }
    reserve_item(lines, &mut budget.bytes)?;
    lines.push(ContentLine { cells, open });
    Ok(())
}

impl ContentProjection {
    /// Content revision captured from the source grid.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Build a single-width row index. Work is bounded by captured cells; memory
    /// is admitted before each index row. Failure preserves the previous index.
    ///
    /// # Errors
    /// Rejects zero width or exhausted work/allocation budgets.
    pub fn prepare(
        &mut self,
        width: usize,
        mut budget: ContentBudget,
    ) -> Result<(), HistorySliceError> {
        if width == 0 {
            return Err(HistorySliceError::InvalidOffset);
        }
        if self
            .index
            .as_ref()
            .is_some_and(|index| index.width == width)
        {
            return Ok(());
        }
        let mut rows = Vec::new();
        for (line, source) in self.lines.iter().enumerate() {
            budget.charge(source.cells.len().max(1), 0)?;
            crate::reflow::visit_logical_rows(&source.cells, width, |range, column, wrapped| {
                reserve_item(&mut rows, &mut budget.bytes)?;
                rows.push(IndexedRow {
                    line,
                    cells: range,
                    column,
                    wrapped: wrapped || source.open,
                });
                Ok(())
            })?;
        }
        self.index = Some(WidthIndex { width, rows });
        Ok(())
    }

    /// Number of rows at the prepared width; unavailable before `prepare`.
    #[must_use]
    pub fn row_count(&self) -> Option<usize> {
        Some(self.index.as_ref()?.rows.len())
    }

    /// Resolve an exact logical-column position at the prepared width.
    /// Anchors from other captures or outside captured content are rejected.
    #[must_use]
    pub fn resolve(&self, anchor: ContentAnchor) -> Option<usize> {
        if anchor.capture != self.capture {
            return None;
        }
        let index = self.index.as_ref()?;
        let end = index
            .rows
            .partition_point(|row| (row.line, row.column) <= (anchor.line, anchor.column));
        let row = index.rows.get(end.checked_sub(1)?)?;
        if row.line != anchor.line {
            return None;
        }
        let source = &self.lines[row.line];
        let mut column = row.column;
        for cell in &source.cells[row.cells.clone()] {
            if column == anchor.column {
                return Some(end - 1);
            }
            column += usize::from(cell.width()).max(1);
        }
        (row.cells.is_empty() && anchor.column == 0).then_some(end - 1)
    }

    /// Project only the selected rows using the prepared index. `bytes` bounds
    /// row/anchor metadata and materialized cell/text allocations.
    ///
    /// # Errors
    /// Returns `Unavailable` before preparation and `BudgetExhausted` on admission failure.
    pub fn window(
        &self,
        range: Range<usize>,
        mut bytes: usize,
    ) -> Result<ContentRows, HistorySliceError> {
        let index = self.index.as_ref().ok_or(HistorySliceError::Unavailable)?;
        let start = range.start.min(index.rows.len());
        let end = range.end.min(index.rows.len()).max(start);
        let count = end - start;
        let metadata = count
            .checked_mul(std::mem::size_of::<PhysicalRow>() + std::mem::size_of::<ContentAnchor>())
            .ok_or(HistorySliceError::BudgetExhausted)?;
        bytes = bytes
            .checked_sub(metadata)
            .ok_or(HistorySliceError::BudgetExhausted)?;
        let mut rows = Vec::new();
        let mut anchors = Vec::new();
        rows.try_reserve_exact(count)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        anchors
            .try_reserve_exact(count)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        for selected in &index.rows[start..end] {
            let source = &self.lines[selected.line];
            // Admission precedes materialization, including wide-cell slots.
            for cell in &source.cells[selected.cells.clone()] {
                charge_projected_cell(&mut bytes, cell)?;
            }
            let mut row = crate::reflow::try_project_logical_line_window_retained(
                &source.cells[selected.cells.clone()],
                index.width,
                0..1,
                true,
            )
            .and_then(|mut rows| rows.pop_front())
            .ok_or(HistorySliceError::BudgetExhausted)?;
            row.set_wrapped(selected.wrapped);
            rows.push(row);
            anchors.push(ContentAnchor {
                capture: self.capture,
                line: selected.line,
                column: selected.column,
            });
        }
        Ok(ContentRows {
            rows,
            anchors,
            has_more_above: self.more_above || start > 0,
            has_more_below: end < index.rows.len(),
            history_truncated: self.history_truncated,
        })
    }

    /// Project a bounded tail after preparation.
    ///
    /// # Errors
    /// Same admission and preparation errors as [`Self::window`].
    pub fn tail(&self, rows: usize, bytes: usize) -> Result<ContentRows, HistorySliceError> {
        let count = self.row_count().ok_or(HistorySliceError::Unavailable)?;
        self.window(count.saturating_sub(rows)..count, bytes)
    }
}

fn charge_projected_cell(bytes: &mut usize, cell: &Cell) -> Result<(), HistorySliceError> {
    let allocation = usize::from(cell.width())
        .max(1)
        .checked_mul(std::mem::size_of::<Cell>() + 1)
        .and_then(|n| n.checked_add(cell.text().len()))
        .ok_or(HistorySliceError::BudgetExhausted)?;
    *bytes = bytes
        .checked_sub(allocation)
        .ok_or(HistorySliceError::BudgetExhausted)?;
    Ok(())
}
