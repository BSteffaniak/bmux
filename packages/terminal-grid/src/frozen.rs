//! Immutable, already projected source rows. No parser replay or inferred history.
use crate::{ContentAnchor, ContentBudget, ContentPosition, HistorySliceError, PhysicalRow};

/// A checked frozen window. Logical columns are source columns, not UTF-8 bytes.
/// The first row may start midway through a line; missing prefixes are never
/// synthesized. Reprojection belongs to the owner of the full logical source.
#[derive(Debug)]
pub struct FrozenContentRows {
    width: usize,
    rows: Vec<PhysicalRow>,
    anchors: Vec<ContentAnchor>,
    retained_bytes: usize,
}

impl FrozenContentRows {
    /// Admit immutable projected rows and their authoritative source origins.
    ///
    /// # Errors
    /// Rejects empty/mismatched origins, discontinuous source, invalid geometry,
    /// and exhausted work or retained-memory allowances.
    pub fn new(
        width: usize,
        rows: Vec<PhysicalRow>,
        anchors: Vec<ContentAnchor>,
        budget: ContentBudget,
    ) -> Result<Self, HistorySliceError> {
        if width == 0 || rows.is_empty() || rows.len() != anchors.len() {
            return Err(HistorySliceError::InvalidOffset);
        }
        let mut cells = 0usize;
        let mut bytes = anchors
            .capacity()
            .saturating_mul(std::mem::size_of::<ContentAnchor>())
            .saturating_add(
                rows.capacity()
                    .saturating_mul(std::mem::size_of::<PhysicalRow>()),
            );
        for (index, row) in rows.iter().enumerate() {
            cells = cells.saturating_add(width.max(row.cells().len()));
            if cells > budget.cells {
                return Err(HistorySliceError::BudgetExhausted);
            }
            bytes = bytes.saturating_add(
                row.allocated_bytes()
                    .ok_or(HistorySliceError::BudgetExhausted)?,
            );
            if cells > budget.cells || bytes > budget.bytes {
                return Err(HistorySliceError::BudgetExhausted);
            }
            if row.cells().len() > width {
                return Err(HistorySliceError::InvalidOffset);
            }
            let mut occupied_until = 0;
            for (column, cell) in row.cells().iter().enumerate() {
                if cell.is_wide_continuation() {
                    if column >= occupied_until {
                        return Err(HistorySliceError::InvalidOffset);
                    }
                } else {
                    if column < occupied_until || cell.width() > 2 {
                        return Err(HistorySliceError::InvalidOffset);
                    }
                    occupied_until = column + usize::from(cell.width());
                    if occupied_until > row.cells().len() && occupied_until <= width {
                        return Err(HistorySliceError::InvalidOffset);
                    }
                }
            }
            let mut end = anchors[index].column;
            for cell in row
                .cells()
                .iter()
                .filter(|cell| !cell.is_wide_continuation())
            {
                end = end
                    .checked_add(usize::from(cell.width()).max(1))
                    .ok_or(HistorySliceError::InvalidOffset)?;
            }
            if let Some(next) = anchors.get(index + 1) {
                let current = anchors[index];
                let continuous = if row.wrapped() {
                    next.line == current.line
                        && next.column >= end
                        && next.column
                            <= current
                                .column
                                .saturating_add(width.max(end - current.column))
                } else {
                    current.line.checked_add(1) == Some(next.line) && next.column == 0
                };
                if current.capture != next.capture || !continuous {
                    return Err(HistorySliceError::InvalidOffset);
                }
            }
        }
        Ok(Self {
            width,
            rows,
            anchors,
            retained_bytes: bytes,
        })
    }

    /// Admitted retained row, text, and origin allocations (allocator overhead excluded).
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    #[must_use]
    pub fn rows(&self) -> &[PhysicalRow] {
        &self.rows
    }

    #[must_use]
    pub fn anchors(&self) -> &[ContentAnchor] {
        &self.anchors
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Resolve a displayed glyph, normalizing continuation slots to its start.
    /// Padding and insertion edges are not selectable cells.
    ///
    /// # Errors
    /// Rejects cells outside the captured source window.
    pub fn source_at(&self, position: ContentPosition) -> Result<ContentAnchor, HistorySliceError> {
        let row = self
            .rows
            .get(position.row)
            .ok_or(HistorySliceError::InvalidOffset)?;
        if position.column >= self.width {
            return Err(HistorySliceError::InvalidOffset);
        }
        let mut anchor = self.anchors[position.row];
        let mut display = 0;
        for cell in row
            .cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
        {
            let width = usize::from(cell.width()).max(1);
            if (display..(display + width).min(self.width)).contains(&position.column) {
                return Ok(anchor);
            }
            display = (display + width).min(self.width);
            anchor.column += width;
        }
        let end = self
            .row_end(position.row)
            .ok_or(HistorySliceError::InvalidOffset)?;
        if position.column >= display && position.column - display < end.column - anchor.column {
            anchor.column += position.column - display;
            return Ok(anchor);
        }
        Err(HistorySliceError::InvalidOffset)
    }

    /// Exclusive logical end of a row (including the complete final glyph).
    #[must_use]
    pub fn row_end(&self, row: usize) -> Option<ContentAnchor> {
        let mut anchor = *self.anchors.get(row)?;
        for cell in self.rows[row]
            .cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
        {
            anchor.column += usize::from(cell.width()).max(1);
        }
        if self.rows[row].wrapped()
            && let Some(next) = self.anchors.get(row + 1)
        {
            return Some(*next);
        }
        Some(anchor)
    }

    /// Resolve only exact glyph boundaries, preferring the next row at a wrap.
    #[must_use]
    pub fn position_of(&self, anchor: ContentAnchor) -> Option<ContentPosition> {
        let mut edge = None;
        for (row, origin) in self.anchors.iter().enumerate() {
            if origin.capture != anchor.capture || origin.line != anchor.line {
                continue;
            }
            let mut logical = origin.column;
            let mut column = 0;
            for cell in self.rows[row]
                .cells()
                .iter()
                .filter(|cell| !cell.is_wide_continuation())
            {
                if logical == anchor.column {
                    return Some(ContentPosition { row, column });
                }
                let width = usize::from(cell.width()).max(1);
                logical += width;
                column = (column + width).min(self.width);
            }
            let end = self.row_end(row)?;
            if anchor.column >= logical && anchor.column <= end.column {
                edge = Some(ContentPosition {
                    row,
                    column: column + anchor.column - logical,
                });
            }
        }
        edge
    }

    /// UTF-8/display correspondence for the visible immutable segments.
    /// Byte zero is the first visible origin of each logical line, not an
    /// invented offset into its missing prefix. Use that origin in content IDs.
    ///
    /// # Errors
    /// Rejects metadata allocations exceeding the explicit allowances.
    pub fn selection_rows(
        &self,
        budget: ContentBudget,
    ) -> Result<Vec<crate::ContentSelectionRow>, HistorySliceError> {
        let mut remaining = budget.bytes;
        let mut work = budget.cells;
        let mut output = Vec::new();
        let mut offset = 0usize;
        for (row, origin) in self.anchors.iter().enumerate() {
            if row == 0 || self.anchors[row - 1].line != origin.line {
                offset = 0;
            }
            let start = offset;
            let mut cells = Vec::new();
            let mut column = 0usize;
            let mut logical = origin.column;
            for cell in self.rows[row]
                .cells()
                .iter()
                .filter(|cell| !cell.is_wide_continuation())
            {
                let end = offset
                    .checked_add(cell.text().len())
                    .ok_or(HistorySliceError::BudgetExhausted)?;
                let width = usize::from(cell.width()).max(1);
                reserve_metadata(&mut cells, &mut remaining)?;
                work = work
                    .checked_sub(1)
                    .ok_or(HistorySliceError::BudgetExhausted)?;
                cells.push(crate::SelectionCell {
                    columns: column..(column + width).min(self.width),
                    bytes: offset..end,
                });
                offset = end;
                column = (column + width).min(self.width);
                logical += width;
            }
            let end = self.row_end(row).ok_or(HistorySliceError::InvalidOffset)?;
            while logical < end.column {
                reserve_metadata(&mut cells, &mut remaining)?;
                work = work
                    .checked_sub(1)
                    .ok_or(HistorySliceError::BudgetExhausted)?;
                cells.push(crate::SelectionCell {
                    columns: column..column + 1,
                    bytes: offset..offset + 1,
                });
                offset += 1;
                column += 1;
                logical += 1;
            }
            reserve_metadata(&mut output, &mut remaining)?;
            output.push(crate::ContentSelectionRow {
                source: crate::ContentRowSource {
                    start: *origin,
                    end,
                    continues: self.rows[row].wrapped(),
                },
                bytes: start..offset,
                cells,
            });
        }
        Ok(output)
    }

    /// Export an ordered half-open logical range from this exact capture.
    /// Soft wraps concatenate; hard breaks insert LF. A prefix/suffix outside
    /// this window, foreign capture, or split-wide-glyph endpoint fails closed.
    /// Effects and cross-window selection remain application-owned.
    ///
    /// # Errors
    /// Rejects invalid endpoints and output exceeding the supplied byte budget.
    pub fn export_text(
        &self,
        start: ContentAnchor,
        end: ContentAnchor,
        max_bytes: usize,
    ) -> Result<String, HistorySliceError> {
        let first = self
            .position_of(start)
            .ok_or(HistorySliceError::InvalidOffset)?;
        let last = self
            .position_of(end)
            .ok_or(HistorySliceError::InvalidOffset)?;
        if start.capture != end.capture || (start.line, start.column) > (end.line, end.column) {
            return Err(HistorySliceError::InvalidOffset);
        }
        let mut result = String::new();
        for row in first.row..=last.row {
            let mut logical = self.anchors[row].column;
            for cell in self.rows[row]
                .cells()
                .iter()
                .filter(|cell| !cell.is_wide_continuation())
            {
                let point = (self.anchors[row].line, logical);
                if point >= (start.line, start.column) && point < (end.line, end.column) {
                    append(&mut result, cell.text(), max_bytes)?;
                }
                logical += usize::from(cell.width()).max(1);
            }
            let row_end = self.row_end(row).ok_or(HistorySliceError::InvalidOffset)?;
            while logical < row_end.column {
                let point = (self.anchors[row].line, logical);
                if point >= (start.line, start.column) && point < (end.line, end.column) {
                    append(&mut result, " ", max_bytes)?;
                }
                logical += 1;
            }
            if row < last.row && !self.rows[row].wrapped() {
                append(&mut result, "\n", max_bytes)?;
            }
        }
        Ok(result)
    }
}

fn reserve_metadata<T>(items: &mut Vec<T>, remaining: &mut usize) -> Result<(), HistorySliceError> {
    *remaining = remaining
        .checked_sub(std::mem::size_of::<T>())
        .ok_or(HistorySliceError::BudgetExhausted)?;
    if items.len() == items.capacity() {
        items
            .try_reserve_exact(1)
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
    }
    Ok(())
}

fn append(output: &mut String, text: &str, budget: usize) -> Result<(), HistorySliceError> {
    if output.len().saturating_add(text.len()) > budget {
        return Err(HistorySliceError::BudgetExhausted);
    }
    output
        .try_reserve(text.len())
        .map_err(|_| HistorySliceError::BudgetExhausted)?;
    output.push_str(text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, ContentProjection, StyleId};

    fn budget() -> ContentBudget {
        ContentBudget {
            cells: 4096,
            bytes: 65536,
        }
    }

    fn frozen(width: usize) -> FrozenContentRows {
        let lines = [
            vec![
                Cell::new("e\u{301}", StyleId::DEFAULT, 1),
                Cell::new("界", StyleId::DEFAULT, 2),
                Cell::new("z", StyleId::DEFAULT, 1),
            ],
            vec![Cell::new("🙂", StyleId::DEFAULT, 2)],
        ];
        let mut projection = ContentProjection::from_lines(
            42,
            7,
            lines.iter().map(|line| (line.as_slice(), false)),
            false,
            false,
            budget(),
        )
        .unwrap();
        projection.prepare(width, budget()).unwrap();
        let window = projection.tail(8, 65536).unwrap();
        FrozenContentRows::new(width, window.rows, window.anchors, budget()).unwrap()
    }

    #[test]
    fn unicode_endpoints_and_soft_wraps_preserve_source() {
        let view = frozen(3);
        let first = view.anchors()[0];
        let wide = view
            .source_at(ContentPosition { row: 0, column: 2 })
            .unwrap();
        assert_eq!(wide.column, 1);
        assert!(
            view.position_of(ContentAnchor { column: 2, ..wide })
                .is_none()
        );
        let end = view.row_end(view.rows().len() - 1).unwrap();
        assert_eq!(
            view.export_text(first, end, 100).unwrap(),
            "e\u{301}界z\n🙂"
        );
        assert!(view.export_text(first, end, 1).is_err());
        assert!(
            view.export_text(
                ContentAnchor {
                    capture: 43,
                    ..first
                },
                end,
                100
            )
            .is_err()
        );
        let wider = frozen(8);
        assert_eq!(
            wider.position_of(wide),
            Some(ContentPosition { row: 0, column: 1 })
        );
    }

    #[test]
    fn discontinuity_and_budget_fail_closed() {
        let mut row = PhysicalRow::new();
        row.set_cell(0, Cell::new("x", StyleId::DEFAULT, 1));
        row.set_wrapped(true);
        let origin = ContentAnchor {
            capture: 1,
            line: 10,
            column: 5,
        };
        assert!(
            FrozenContentRows::new(
                2,
                vec![row.clone(), row.clone()],
                vec![
                    origin,
                    ContentAnchor {
                        column: 9,
                        ..origin
                    }
                ],
                budget()
            )
            .is_err()
        );
        assert!(
            FrozenContentRows::new(
                2,
                vec![row],
                vec![origin],
                ContentBudget {
                    cells: 1,
                    bytes: 65536
                }
            )
            .is_err()
        );
    }

    #[test]
    fn trimmed_significant_blanks_use_authoritative_next_origin() {
        let mut row = PhysicalRow::new();
        row.set_cell(0, Cell::new("x", StyleId::DEFAULT, 1));
        row.set_wrapped(true);
        let origin = ContentAnchor {
            capture: 1,
            line: 10,
            column: 5,
        };
        let next = ContentAnchor {
            column: 8,
            ..origin
        };
        let view = FrozenContentRows::new(
            3,
            vec![row, PhysicalRow::new()],
            vec![origin, next],
            budget(),
        )
        .unwrap();
        assert_eq!(view.export_text(origin, next, 100).unwrap(), "x  ");
        assert_eq!(
            view.source_at(ContentPosition { row: 0, column: 2 })
                .unwrap()
                .column,
            7
        );
        let metadata = view.selection_rows(budget()).unwrap();
        assert_eq!(metadata[0].bytes, 0..3);
        assert_eq!(metadata[0].cells.last().unwrap().bytes, 2..3);
        assert_eq!(metadata[1].bytes, 3..3);
        assert_eq!(view.position_of(next).unwrap().row, 1);
    }
}
