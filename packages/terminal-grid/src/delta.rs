use crate::model::ProtocolState;
use crate::snapshot::{CursorSnapshot, GridSnapshot, RowSnapshot, ScrollRegionSnapshot};
use crate::style::Style;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One changed retained row in a structured grid delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowUpdateSnapshot {
    /// Row index within the retained snapshot row set after applying the delta.
    pub row_index: u32,
    pub row: RowSnapshot,
}

/// Revisioned structured terminal update.
#[allow(
    clippy::struct_excessive_bools,
    reason = "serialized grid delta wire state intentionally carries independent terminal mode flags"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridDeltaBatch {
    /// Revision the receiver must already have before applying this delta.
    pub base_revision: u64,
    /// Revision after applying this delta.
    pub revision: u64,
    #[serde(default)]
    pub content_revision: u64,
    pub width: u16,
    pub height: u16,
    pub mode: String,
    pub scrollback_rows: u32,
    pub cursor: CursorSnapshot,
    #[serde(default)]
    pub saved_cursor: CursorSnapshot,
    #[serde(default)]
    pub saved_pending_wrap: bool,
    #[serde(default)]
    pub characters: crate::CharacterState,
    #[serde(default)]
    pub saved_characters: crate::CharacterState,
    #[serde(default)]
    pub current_style: Style,
    #[serde(default = "default_autowrap")]
    pub autowrap: bool,
    #[serde(default)]
    pub pending_wrap: bool,
    #[serde(default)]
    pub scroll_region: Option<ScrollRegionSnapshot>,
    #[serde(default)]
    pub protocol: ProtocolState,
    #[serde(default)]
    pub pending_bytes: Vec<u8>,
    pub styles: Vec<Style>,
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
            content_revision: after.content_revision,
            width: after.width,
            height: after.height,
            mode: after.mode.clone(),
            scrollback_rows: after.scrollback_rows,
            cursor: after.cursor,
            saved_cursor: after.saved_cursor,
            saved_pending_wrap: after.saved_pending_wrap,
            characters: after.characters,
            saved_characters: after.saved_characters,
            current_style: after.current_style,
            autowrap: after.autowrap,
            pending_wrap: after.pending_wrap,
            scroll_region: after.scroll_region,
            protocol: after.protocol,
            pending_bytes: after.pending_bytes.clone(),
            styles: after.styles.clone(),
            reset_rows,
            row_updates,
        })
    }

    /// Apply this delta to a retained grid snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the receiver does not have `base_revision` or a
    /// row update indexes outside the current row set.
    pub fn apply_to_snapshot(
        &self,
        snapshot: &mut GridSnapshot,
    ) -> Result<(), GridDeltaApplyError> {
        if snapshot.revision != self.base_revision {
            return Err(GridDeltaApplyError::RevisionMismatch {
                expected: self.base_revision,
                actual: snapshot.revision,
            });
        }
        if self.reset_rows {
            snapshot.rows = self
                .row_updates
                .iter()
                .map(|update| update.row.clone())
                .collect();
        } else {
            for update in &self.row_updates {
                let index = usize::try_from(update.row_index).unwrap_or(usize::MAX);
                let Some(row) = snapshot.rows.get_mut(index) else {
                    return Err(GridDeltaApplyError::RowIndexOutOfBounds(update.row_index));
                };
                *row = update.row.clone();
            }
        }
        snapshot.revision = self.revision;
        snapshot.content_revision = self.content_revision;
        snapshot.width = self.width;
        snapshot.height = self.height;
        snapshot.mode.clone_from(&self.mode);
        snapshot.scrollback_rows = self.scrollback_rows;
        snapshot.cursor = self.cursor;
        snapshot.saved_cursor = self.saved_cursor;
        snapshot.saved_pending_wrap = self.saved_pending_wrap;
        snapshot.characters = self.characters;
        snapshot.saved_characters = self.saved_characters;
        snapshot.current_style = self.current_style;
        snapshot.autowrap = self.autowrap;
        snapshot.pending_wrap = self.pending_wrap;
        snapshot.scroll_region = self.scroll_region;
        snapshot.protocol = self.protocol;
        snapshot.pending_bytes.clone_from(&self.pending_bytes);
        snapshot.styles.clone_from(&self.styles);
        Ok(())
    }
}

const fn default_autowrap() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GridDeltaApplyError {
    #[error("grid delta base revision mismatch: expected {expected}, actual {actual}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("grid delta row index {0} is outside the retained row set")]
    RowIndexOutOfBounds(u32),
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

    #[test]
    fn delta_applies_to_snapshot() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        let mut snapshot = grid.snapshot(0, 10);
        grid.process(b"hello");
        let after = grid.snapshot(0, 10);
        let delta = GridDeltaBatch::between(&snapshot, &after).expect("revision changed");

        delta
            .apply_to_snapshot(&mut snapshot)
            .expect("delta should apply to base snapshot");

        assert_eq!(snapshot, after);
    }
}
