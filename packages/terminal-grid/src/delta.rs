use crate::snapshot::{CursorSnapshot, GridSnapshot, RowSnapshot};
use serde::{Deserialize, Serialize};

/// One changed retained row in a structured grid delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowUpdateSnapshot {
    /// Row index within the retained snapshot row set after applying the delta.
    pub row_index: u32,
    pub row: RowSnapshot,
}

/// Revisioned structured terminal update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridDeltaBatch {
    /// Revision the receiver must already have before applying this delta.
    pub base_revision: u64,
    /// Revision after applying this delta.
    pub revision: u64,
    pub width: u16,
    pub height: u16,
    pub mode: String,
    pub scrollback_rows: u32,
    pub cursor: CursorSnapshot,
    /// True when row indexes or dimensions changed enough that receivers should
    /// discard their local row set before applying `row_updates`.
    pub reset_rows: bool,
    pub row_updates: Vec<RowUpdateSnapshot>,
}

impl GridDeltaBatch {
    #[must_use]
    pub fn between(before: &GridSnapshot, after: &GridSnapshot) -> Option<Self> {
        if before.revision == after.revision {
            return None;
        }
        let reset_rows = before.width != after.width
            || before.height != after.height
            || before.mode != after.mode
            || before.scrollback_rows != after.scrollback_rows
            || before.rows.len() != after.rows.len();
        let row_updates = if reset_rows {
            after
                .rows
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, row)| RowUpdateSnapshot {
                    row_index: u32::try_from(index).unwrap_or(u32::MAX),
                    row,
                })
                .collect()
        } else {
            before
                .rows
                .iter()
                .zip(&after.rows)
                .enumerate()
                .filter(|(_, (old, new))| old != new)
                .map(|(index, (_, new))| RowUpdateSnapshot {
                    row_index: u32::try_from(index).unwrap_or(u32::MAX),
                    row: new.clone(),
                })
                .collect()
        };
        Some(Self {
            base_revision: before.revision,
            revision: after.revision,
            width: after.width,
            height: after.height,
            mode: after.mode.clone(),
            scrollback_rows: after.scrollback_rows,
            cursor: after.cursor,
            reset_rows,
            row_updates,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GridLimits, TerminalGrid};

    #[test]
    fn delta_reports_changed_rows() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        let before = grid.snapshot(0, 10);
        grid.process(b"hello");
        let after = grid.snapshot(0, 10);

        let delta = GridDeltaBatch::between(&before, &after).expect("revision changed");

        assert!(!delta.reset_rows);
        assert_eq!(delta.row_updates.len(), 1);
        assert_eq!(delta.row_updates[0].row_index, 0);
    }

    #[test]
    fn delta_resets_rows_when_dimensions_change() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"abcdef");
        let before = grid.snapshot(0, 10);
        grid.resize(4, 2).unwrap();
        let after = grid.snapshot(0, 10);

        let delta = GridDeltaBatch::between(&before, &after).expect("revision changed");

        assert!(delta.reset_rows);
        assert_eq!(delta.row_updates.len(), after.rows.len());
    }
}
