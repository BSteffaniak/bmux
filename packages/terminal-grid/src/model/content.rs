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

/// Source extent of one projected row. Both anchors address the same logical
/// line; `end` is exclusive, including when a wide glyph occupies one display
/// column. An empty line has equal start/end anchors but is still a visible row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRowSource {
    pub start: ContentAnchor,
    pub end: ContentAnchor,
    /// The row continues into another projected row or outside this capture.
    pub continues: bool,
}

/// One complete source cell's display geometry and UTF-8 byte interval.
/// Byte coordinates are local to the identified logical line (or screen row).
/// Combining characters stay with their owning cell; clipped screen fragments
/// have no mapping. Display and source widths need not agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionCell {
    pub columns: Range<usize>,
    pub bytes: Range<usize>,
}

/// Selection correspondence for one projected row.
#[derive(Debug)]
pub struct ContentSelectionRow {
    pub source: ContentRowSource,
    pub bytes: Range<usize>,
    pub cells: Vec<SelectionCell>,
}

/// A positioned screen row's source coordinates. Display column `x` maps to
/// `columns.start + x`, including implicit blank cells. A clipped wide-glyph
/// fragment is blanked, not a selectable partial source glyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenRowSource {
    pub row: usize,
    pub columns: Range<usize>,
    pub clipped_left: bool,
    pub clipped_right: bool,
}

/// Selection-ready positioned row. `text` is the admitted source interval;
/// `byte_start` locates it in the full physical source row. Mappings omit
/// clipped wide-cell fragments and include implicit blanks inside the screen.
#[derive(Debug)]
pub struct ScreenSelectionRow {
    pub source: ScreenRowSource,
    pub byte_start: usize,
    pub text: String,
    pub cells: Vec<SelectionCell>,
}

/// Bounded active-screen crop with correspondence from the same grid revision.
#[derive(Debug)]
pub struct ScreenRows {
    pub rows: Vec<PhysicalRow>,
    pub sources: Vec<ScreenRowSource>,
    pub revision: u64,
}

/// A bounded projected window, with source positions for its row starts.
#[derive(Debug)]
pub struct ContentRows {
    pub rows: Vec<PhysicalRow>,
    pub anchors: Vec<ContentAnchor>,
    /// One source range per row, in exactly the same order as `rows`/`anchors`.
    pub sources: Vec<ContentRowSource>,
    pub has_more_above: bool,
    pub has_more_below: bool,
    /// The source grid has already evicted history. No missing prefix is invented.
    pub history_truncated: bool,
}

#[derive(Debug)]
struct ContentLine {
    cells: Vec<Cell>,
    byte_offsets: Vec<usize>,
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
    viewport_prefix_continues: bool,
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
                copy_physical_row(&mut cells, row, self.width, &mut budget)?;
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
            viewport_prefix_continues: false,
            index: None,
        })
    }

    /// Capture only the main-screen viewport for a live transcript.
    ///
    /// Soft-wrapped rows inside the viewport remain joined. A continuation from
    /// history starts a capture-local logical fragment at column zero: hidden
    /// progress/history cells are neither copied nor revealed when widening.
    /// [`ContentProjection::viewport_prefix_continues`] reports that boundary.
    /// The final blank cursor row is retained, unlike completed content capture.
    /// Alternate screens are positioned output; use [`Self::screen_window`].
    ///
    /// # Errors
    /// Returns `Unavailable` in alternate mode and `BudgetExhausted` when the
    /// viewport exceeds the supplied work or allocation allowances.
    pub fn capture_viewport(
        &self,
        capture: u128,
        mut budget: ContentBudget,
    ) -> Result<ContentProjection, HistorySliceError> {
        if self.mode != GridMode::Main {
            return Err(HistorySliceError::Unavailable);
        }
        let mut end = self.main_rows.len().min(self.height);
        while end > self.cursor.row.saturating_add(1) {
            let row = &self.main_rows[end - 1];
            budget.charge(row.cells().len().max(1), 0)?;
            if !row.cells().is_empty() {
                break;
            }
            end -= 1;
        }
        let mut lines = Vec::new();
        let mut cells = Vec::new();
        for (index, row) in self.main_rows.range(..end).enumerate() {
            copy_physical_row(&mut cells, row, self.width, &mut budget)?;
            if !row.wrapped() || index + 1 == end {
                push_line(
                    &mut lines,
                    std::mem::take(&mut cells),
                    row.wrapped(),
                    &mut budget,
                )?;
            }
        }
        Ok(ContentProjection {
            capture,
            revision: self.content_revision,
            lines,
            more_above: !self.main_history.is_empty() || !self.pending_history_cells.is_empty(),
            history_truncated: self.history_truncated,
            viewport_prefix_continues: !self.pending_history_cells.is_empty(),
            index: None,
        })
    }

    /// Capture selection text and geometry for active positioned screen rows.
    ///
    /// Requires the exact content revision. UTF-8 coordinates count source cells
    /// from column zero, including implicit blanks. Scanning to the requested
    /// columns is charged; source prefixes are never copied. Clipped wide-cell
    /// fragments are excluded from both exported text and selectable geometry.
    ///
    /// # Errors
    /// Rejects stale revisions and exhausted work/allocation allowances.
    pub fn screen_selection(
        &self,
        revision: u64,
        columns: Range<usize>,
        rows: Range<usize>,
        mut budget: ContentBudget,
    ) -> Result<Vec<ScreenSelectionRow>, HistorySliceError> {
        if revision != self.content_revision {
            return Err(HistorySliceError::StaleRevision);
        }
        let start = columns.start.min(self.width);
        let end = columns.end.min(self.width).max(start);
        let mut output = Vec::new();
        for y in rows.start.min(self.height)..rows.end.min(self.height) {
            budget.charge(end.max(1), 0)?;
            let row = self
                .viewport_row_ref(y)
                .ok_or(HistorySliceError::Unavailable)?;
            reserve_item(&mut output, &mut budget.bytes)?;
            let source = ScreenRowSource {
                row: y,
                columns: start..end,
                clipped_left: start < end
                    && row
                        .cells()
                        .get(start)
                        .is_some_and(Cell::is_wide_continuation),
                clipped_right: start < end
                    && row.cells().get(end).is_some_and(Cell::is_wide_continuation),
            };
            let mut selected = ScreenSelectionRow {
                source,
                byte_start: 0,
                text: String::new(),
                cells: Vec::new(),
            };
            let mut offset = 0_usize;
            for x in 0..end {
                let cell = row.cells().get(x);
                if cell.is_some_and(Cell::is_wide_continuation) {
                    continue;
                }
                let text = cell.map_or(" ", Cell::text);
                let width = cell.map_or(1, |cell| usize::from(cell.width()).max(1));
                let next = offset
                    .checked_add(text.len())
                    .ok_or(HistorySliceError::BudgetExhausted)?;
                if x >= start && x.saturating_add(width) <= end {
                    if selected.cells.is_empty() {
                        selected.byte_start = offset;
                    }
                    reserve_item(&mut selected.cells, &mut budget.bytes)?;
                    budget.charge(0, text.len())?;
                    selected
                        .text
                        .try_reserve_exact(text.len())
                        .map_err(|_| HistorySliceError::BudgetExhausted)?;
                    selected.text.push_str(text);
                    selected.cells.push(SelectionCell {
                        columns: x - start..x - start + width,
                        bytes: offset..next,
                    });
                }
                offset = next;
            }
            if selected.cells.is_empty() {
                selected.byte_start = offset;
            }
            output.push(selected);
        }
        Ok(output)
    }

    /// Crop positioned output and return source-column correspondence.
    /// No terminal state is changed. Metadata is admitted before row allocation.
    /// A source range describes physical columns, not UTF-8 bytes; edge flags
    /// mark blanked fragments of wide glyphs that selection must not copy.
    ///
    /// # Errors
    /// Returns `BudgetExhausted` when metadata, scanning, or row allocations
    /// exceed the caller's allowances.
    pub fn screen_window_with_sources(
        &self,
        columns: Range<usize>,
        rows: Range<usize>,
        mut budget: ContentBudget,
    ) -> Result<ScreenRows, HistorySliceError> {
        let start = columns.start.min(self.width);
        let end = columns.end.min(self.width).max(start);
        let first_row = rows.start.min(self.height);
        let last_row = rows.end.min(self.height).max(first_row);
        let count = last_row - first_row;
        let metadata = count
            .checked_mul(std::mem::size_of::<ScreenRowSource>())
            .ok_or(HistorySliceError::BudgetExhausted)?;
        budget.charge(count, metadata)?;
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(count)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        for row in first_row..last_row {
            let source = self
                .viewport_row_ref(row)
                .ok_or(HistorySliceError::Unavailable)?;
            sources.push(ScreenRowSource {
                row,
                columns: start..end,
                clipped_left: start < end
                    && source
                        .cells()
                        .get(start)
                        .is_some_and(Cell::is_wide_continuation),
                clipped_right: start < end
                    && source
                        .cells()
                        .get(end)
                        .is_some_and(Cell::is_wide_continuation),
            });
        }
        let rows = self.screen_window(start..end, first_row..last_row, budget)?;
        Ok(ScreenRows {
            rows,
            sources,
            revision: self.content_revision,
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

fn copy_physical_row(
    cells: &mut Vec<Cell>,
    row: &PhysicalRow,
    width: usize,
    budget: &mut ContentBudget,
) -> Result<(), HistorySliceError> {
    // Soft continuations include implicit padding at execution width.
    let columns = if row.wrapped() {
        width
    } else {
        row.cells().len()
    };
    budget.charge(columns.max(1), 0)?;
    for col in 0..columns {
        if let Some(cell) = row.cells().get(col) {
            if !cell.is_wide_continuation() {
                copy_cell(cells, cell, budget)?;
            }
        } else {
            copy_cell(cells, &Cell::blank(crate::StyleId::DEFAULT), budget)?;
        }
    }
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
    let count = cells
        .len()
        .checked_add(1)
        .ok_or(HistorySliceError::BudgetExhausted)?;
    budget.charge(
        cells.len(),
        count
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or(HistorySliceError::BudgetExhausted)?,
    )?;
    let mut byte_offsets = Vec::new();
    byte_offsets
        .try_reserve_exact(count)
        .map_err(|_| HistorySliceError::BudgetExhausted)?;
    byte_offsets.push(0_usize);
    for cell in &cells {
        let end = byte_offsets
            .last()
            .copied()
            .unwrap_or(0)
            .checked_add(cell.text().len())
            .ok_or(HistorySliceError::BudgetExhausted)?;
        byte_offsets.push(end);
    }
    reserve_item(lines, &mut budget.bytes)?;
    lines.push(ContentLine {
        cells,
        byte_offsets,
        open,
    });
    Ok(())
}

impl ContentProjection {
    /// Whether a viewport capture begins mid-line with its prefix outside the
    /// viewport. The first visible fragment's logical origin is `(line: 0,
    /// column: 0)` in this capture, not an offset into hidden history.
    #[must_use]
    pub const fn viewport_prefix_continues(&self) -> bool {
        self.viewport_prefix_continues
    }

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

    /// Return per-cell UTF-8 correspondence for a prepared row window.
    ///
    /// Coordinates are capture/line scoped and stable across width changes.
    /// Soft wraps never introduce bytes. Hard breaks are represented by
    /// `source.continues == false`; callers insert separators between logical
    /// lines, not between projected rows. Empty lines retain an empty interval.
    ///
    /// # Errors
    /// Returns `Unavailable` before preparation or `BudgetExhausted` when work
    /// or metadata exceeds the explicit allowance. No prefix scanning occurs.
    pub fn selection_window(
        &self,
        range: Range<usize>,
        mut budget: ContentBudget,
    ) -> Result<Vec<ContentSelectionRow>, HistorySliceError> {
        let index = self.index.as_ref().ok_or(HistorySliceError::Unavailable)?;
        let start = range.start.min(index.rows.len());
        let end = range.end.min(index.rows.len()).max(start);
        let mut output = Vec::new();
        for row in &index.rows[start..end] {
            budget.charge(row.cells.len().max(1), 0)?;
            reserve_item(&mut output, &mut budget.bytes)?;
            let line = &self.lines[row.line];
            let mut cells = Vec::new();
            let mut column = 0_usize;
            let mut logical_end = row.column;
            for i in row.cells.clone() {
                let cell = &line.cells[i];
                let width = usize::from(cell.width()).max(1);
                let end = column.saturating_add(width).min(index.width);
                reserve_item(&mut cells, &mut budget.bytes)?;
                cells.push(SelectionCell {
                    columns: column..end,
                    bytes: line.byte_offsets[i]..line.byte_offsets[i + 1],
                });
                column = end;
                logical_end += width;
            }
            let anchor = ContentAnchor {
                capture: self.capture,
                line: row.line,
                column: row.column,
            };
            output.push(ContentSelectionRow {
                source: ContentRowSource {
                    start: anchor,
                    end: ContentAnchor {
                        column: logical_end,
                        ..anchor
                    },
                    continues: row.wrapped,
                },
                bytes: line.byte_offsets[row.cells.start]..line.byte_offsets[row.cells.end],
                cells,
            });
        }
        Ok(output)
    }

    /// Export a cell-aligned UTF-8 interval from one captured logical line.
    /// No newlines are synthesized, and selection never splits a combining
    /// sequence or wide glyph. Callers own hard-line separator policy.
    ///
    /// # Errors
    /// Rejects a foreign capture, missing line, non-cell boundaries, reversed
    /// ranges, or insufficient work/text allocation allowance.
    pub fn export_text(
        &self,
        capture: u128,
        line: usize,
        bytes: Range<usize>,
        mut budget: ContentBudget,
    ) -> Result<String, HistorySliceError> {
        if capture != self.capture {
            return Err(HistorySliceError::StaleRevision);
        }
        let line = self.lines.get(line).ok_or(HistorySliceError::Unavailable)?;
        if bytes.start > bytes.end {
            return Err(HistorySliceError::InvalidOffset);
        }
        let start = line
            .byte_offsets
            .binary_search(&bytes.start)
            .map_err(|_| HistorySliceError::InvalidOffset)?;
        let end = line
            .byte_offsets
            .binary_search(&bytes.end)
            .map_err(|_| HistorySliceError::InvalidOffset)?;
        budget.charge(end - start, bytes.end - bytes.start)?;
        let mut text = String::new();
        text.try_reserve_exact(bytes.end - bytes.start)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        for cell in &line.cells[start..end] {
            text.push_str(cell.text());
        }
        Ok(text)
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
            .checked_mul(
                std::mem::size_of::<PhysicalRow>()
                    + std::mem::size_of::<ContentAnchor>()
                    + std::mem::size_of::<ContentRowSource>(),
            )
            .ok_or(HistorySliceError::BudgetExhausted)?;
        bytes = bytes
            .checked_sub(metadata)
            .ok_or(HistorySliceError::BudgetExhausted)?;
        let mut rows = Vec::new();
        let mut anchors = Vec::new();
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(count)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        rows.try_reserve_exact(count)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        anchors
            .try_reserve_exact(count)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        for selected in &index.rows[start..end] {
            let source = &self.lines[selected.line];
            // Admission precedes materialization, including wide-cell slots.
            let mut end_column = selected.column;
            for cell in &source.cells[selected.cells.clone()] {
                charge_projected_cell(&mut bytes, cell)?;
                end_column = end_column
                    .checked_add(usize::from(cell.width()).max(1))
                    .ok_or(HistorySliceError::BudgetExhausted)?;
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
            let anchor = ContentAnchor {
                capture: self.capture,
                line: selected.line,
                column: selected.column,
            };
            anchors.push(anchor);
            sources.push(ContentRowSource {
                start: anchor,
                end: ContentAnchor {
                    column: end_column,
                    ..anchor
                },
                continues: selected.wrapped,
            });
        }
        Ok(ContentRows {
            rows,
            anchors,
            sources,
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
