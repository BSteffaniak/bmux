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
    /// Authoritative cumulative scroll position; absent on legacy senders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_scrolled_rows: Option<u64>,
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
    /// Replace hidden main-screen rows only when they change, not on each frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_rows: Option<Vec<RowSnapshot>>,
}

impl GridDeltaBatch {
    /// Build an update only when replication advances and content does not regress.
    #[must_use]
    pub fn between(before: &GridSnapshot, after: &GridSnapshot) -> Option<Self> {
        if after.revision <= before.revision || after.content_revision < before.content_revision {
            return None;
        }
        let reset_rows = before.width != after.width
            || before.height != after.height
            || before.mode != after.mode
            || before.scrollback_rows != after.scrollback_rows
            || before.total_scrolled_rows != after.total_scrolled_rows
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
            total_scrolled_rows: after.total_scrolled_rows,
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
            main_rows: (before.main_rows != after.main_rows)
                .then(|| after.main_rows.clone())
                .flatten(),
        })
    }

    /// Validate ordering without materializing the receiver's retained content.
    pub(crate) fn validate_revisions(
        &self,
        revision: u64,
        content_revision: u64,
    ) -> Result<(), GridDeltaApplyError> {
        if revision != self.base_revision {
            return Err(GridDeltaApplyError::RevisionMismatch {
                expected: self.base_revision,
                actual: revision,
            });
        }
        if self.revision <= self.base_revision {
            return Err(GridDeltaApplyError::NonAdvancingRevision {
                base: self.base_revision,
                revision: self.revision,
            });
        }
        if self.content_revision < content_revision {
            return Err(GridDeltaApplyError::RegressingContentRevision {
                current: content_revision,
                received: self.content_revision,
            });
        }
        Ok(())
    }

    /// Validate dimensions and screen identity before materializing retained rows.
    pub(crate) fn validate_geometry(&self) -> Result<(), GridDeltaApplyError> {
        if self.width == 0 || self.height == 0 {
            return Err(GridDeltaApplyError::ZeroDimensions);
        }
        if !matches!(self.mode.as_str(), "main" | "alternate") {
            return Err(GridDeltaApplyError::InvalidScreenMode);
        }
        // Omitted backing preserves the receiver's state for legacy and sparse
        // updates. Explicit replacement backing must contain its full viewport.
        if self.mode == "alternate"
            && let Some(rows) = &self.main_rows
            && rows.len() < usize::from(self.height)
        {
            return Err(GridDeltaApplyError::IncompleteMainViewport {
                expected: self.height,
                actual: rows.len(),
            });
        }
        Ok(())
    }

    /// Validate replacement indexes without allocating the replacement row set.
    pub(crate) fn validate_replacement_indexes(&self) -> Result<(), GridDeltaApplyError> {
        if self.reset_rows {
            if self.row_updates.len() < usize::from(self.height) {
                return Err(GridDeltaApplyError::IncompleteViewport {
                    expected: self.height,
                    actual: self.row_updates.len(),
                });
            }
            for (expected, update) in self.row_updates.iter().enumerate() {
                if usize::try_from(update.row_index).ok() != Some(expected) {
                    return Err(GridDeltaApplyError::InvalidReplacementRowIndex {
                        expected,
                        actual: update.row_index,
                    });
                }
            }
        }
        Ok(())
    }

    /// Apply this delta to a retained grid snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the receiver does not have `base_revision`, the
    /// revision does not advance, dimensions or screen mode are invalid, a screen
    /// or size change lacks replacement rows, a row update indexes outside the
    /// current row set, sparse indexes are not strictly increasing, or replacement
    /// rows are not indexed consecutively from zero.
    /// Errors leave the snapshot unchanged.
    pub fn apply_to_snapshot(
        &self,
        snapshot: &mut GridSnapshot,
    ) -> Result<(), GridDeltaApplyError> {
        self.validate_revisions(snapshot.revision, snapshot.content_revision)?;
        self.validate_geometry()?;
        if !self.reset_rows
            && (self.width != snapshot.width
                || self.height != snapshot.height
                || self.scrollback_rows != snapshot.scrollback_rows
                || self
                    .total_scrolled_rows
                    .is_some_and(|position| snapshot.total_scrolled_rows != Some(position))
                || self.mode != snapshot.mode)
        {
            return Err(GridDeltaApplyError::MissingReplacementRows);
        }
        if self.reset_rows {
            self.validate_replacement_indexes()?;
            snapshot.rows = self
                .row_updates
                .iter()
                .map(|update| update.row.clone())
                .collect();
        } else {
            // Validate the complete batch before mutating any rows so recovery can
            // retry from the unchanged base snapshot after a malformed update.
            let mut previous_index = None;
            for update in &self.row_updates {
                let index = usize::try_from(update.row_index).unwrap_or(usize::MAX);
                if index >= snapshot.rows.len() {
                    return Err(GridDeltaApplyError::RowIndexOutOfBounds(update.row_index));
                }
                if previous_index.is_some_and(|previous| previous >= index) {
                    return Err(GridDeltaApplyError::NonIncreasingRowIndex(update.row_index));
                }
                previous_index = Some(index);
            }
            for update in &self.row_updates {
                let index = usize::try_from(update.row_index).unwrap_or(usize::MAX);
                snapshot.rows[index].clone_from(&update.row);
            }
        }
        if self.mode != "alternate" {
            snapshot.main_rows = None;
        } else if let Some(rows) = &self.main_rows {
            snapshot.main_rows = Some(rows.clone());
        }
        snapshot.revision = self.revision;
        snapshot.content_revision = self.content_revision;
        snapshot.width = self.width;
        snapshot.height = self.height;
        snapshot.mode.clone_from(&self.mode);
        snapshot.scrollback_rows = self.scrollback_rows;
        snapshot.total_scrolled_rows = self.total_scrolled_rows;
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
    #[error("grid delta row index {0} does not strictly increase")]
    NonIncreasingRowIndex(u32),
    #[error("grid delta dimensions must be nonzero")]
    ZeroDimensions,
    #[error("grid delta has an unknown screen mode")]
    InvalidScreenMode,
    #[error("grid delta changes screen, dimensions, or scrollback count without replacement rows")]
    MissingReplacementRows,
    #[error("grid delta base revision mismatch: expected {expected}, actual {actual}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("grid delta revision {revision} does not advance base {base}")]
    NonAdvancingRevision { base: u64, revision: u64 },
    #[error("grid delta content revision {received} precedes current {current}")]
    RegressingContentRevision { current: u64, received: u64 },
    #[error("grid delta row index {0} is outside the retained row set")]
    RowIndexOutOfBounds(u32),
    #[error("replacement main backing needs at least {expected} rows, received {actual}")]
    IncompleteMainViewport { expected: u16, actual: usize },
    #[error("replacement viewport needs at least {expected} rows, received {actual}")]
    IncompleteViewport { expected: u16, actual: usize },
    #[error("replacement row index mismatch: expected {expected}, actual {actual}")]
    InvalidReplacementRowIndex { expected: usize, actual: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GridLimits, TerminalGrid};

    #[test]
    fn between_rejects_content_regression_but_allows_metadata_only_updates() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"main");
        let before = grid.snapshot(0, 3);
        grid.process(b"\x1b7");
        let after = grid.snapshot(0, 3);
        assert!(after.revision > before.revision);
        assert_eq!(after.content_revision, before.content_revision);
        let mut invalid = after.clone();
        invalid.content_revision = before.content_revision - 1;
        assert!(GridDeltaBatch::between(&before, &invalid).is_none());
        let mut replica = before.clone();
        GridDeltaBatch::between(&before, &after)
            .unwrap()
            .apply_to_snapshot(&mut replica)
            .unwrap();
        assert_eq!(replica, after);
    }

    #[test]
    fn metadata_delta_does_not_retransmit_retained_history() {
        let mut grid = TerminalGrid::new(
            10,
            3,
            GridLimits {
                scrollback_rows: 2_000,
            },
        )
        .unwrap();
        grid.process(&vec![b'x'; 10_000]);
        let mut replica = grid.snapshot(0, 2_003);
        assert!(replica.rows.len() > 900);
        grid.process(b"\x1b[1;1H");
        let after = grid.snapshot(0, 2_003);
        let delta = GridDeltaBatch::between(&replica, &after).unwrap();
        assert!(!delta.reset_rows);
        assert!(delta.row_updates.is_empty());
        assert!(delta.main_rows.is_none());
        assert_eq!(delta.content_revision, replica.content_revision);
        delta.apply_to_snapshot(&mut replica).unwrap();
        assert_eq!(replica, after);

        grid.process(b"\x1b[?1049halt");
        let alternate = grid.snapshot(0, 2_003);
        assert!(
            alternate
                .main_rows
                .as_ref()
                .is_some_and(|rows| !rows.is_empty())
        );
        GridDeltaBatch::between(&replica, &alternate)
            .unwrap()
            .apply_to_snapshot(&mut replica)
            .unwrap();
        for bytes in [b"\x1b[1;1H".as_slice(), b"changed"] {
            grid.process(bytes);
            let after = grid.snapshot(0, 2_003);
            let delta = GridDeltaBatch::between(&replica, &after).unwrap();
            assert!(!delta.reset_rows);
            assert!(delta.main_rows.is_none());
            delta.apply_to_snapshot(&mut replica).unwrap();
            assert_eq!(replica, after);
        }

        grid.process(b"\x1b[?1049l");
        let restored = grid.snapshot(0, 2_003);
        let delta = GridDeltaBatch::between(&replica, &restored).unwrap();
        assert!(delta.main_rows.is_none());
        assert_eq!(delta.mode, "main");
        // The screen transition clears backing rows without retransmitting them;
        // verify that behavior survives the wire round-trip.
        let encoded = serde_json::to_vec(&delta).unwrap();
        let decoded: GridDeltaBatch = serde_json::from_slice(&encoded).unwrap();
        decoded.apply_to_snapshot(&mut replica).unwrap();
        assert_eq!(replica, restored);
        assert!(replica.main_rows.is_none());
    }

    #[test]
    fn between_rejects_reversed_and_unchanged_snapshots() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"main");
        let before = grid.snapshot(0, 3);
        grid.process(b"\x1b[?1049halt");
        let after = grid.snapshot(0, 3);
        assert!(GridDeltaBatch::between(&after, &before).is_none());
        assert!(GridDeltaBatch::between(&after, &after).is_none());
        let mut replica = before.clone();
        GridDeltaBatch::between(&before, &after)
            .unwrap()
            .apply_to_snapshot(&mut replica)
            .unwrap();
        assert_eq!(replica, after);
    }

    #[test]
    fn scrollback_count_change_requires_replacement_rows() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"one\r\ntwo\r\nthree");
        let before = grid.snapshot(0, 10);
        grid.process(b"\r\nfour");
        let after = grid.snapshot(0, 10);
        assert_ne!(before.scrollback_rows, after.scrollback_rows);
        let delta = GridDeltaBatch::between(&before, &after).unwrap();
        assert!(delta.reset_rows);
        let mut malformed = delta.clone();
        malformed.reset_rows = false;
        malformed.row_updates.clear();
        let mut snapshot = before.clone();
        assert_eq!(
            malformed.apply_to_snapshot(&mut snapshot),
            Err(GridDeltaApplyError::MissingReplacementRows)
        );
        assert_eq!(snapshot, before);
        delta.apply_to_snapshot(&mut snapshot).unwrap();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn regressing_content_revision_preserves_snapshot_and_allows_retry() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"main");
        let before = grid.snapshot(0, 3);
        grid.process(b"\x1b[?1049halt");
        let after = grid.snapshot(0, 3);
        let delta = GridDeltaBatch::between(&before, &after).unwrap();
        let mut malformed = delta.clone();
        malformed.content_revision = before.content_revision - 1;
        let mut snapshot = before.clone();
        assert_eq!(
            malformed.apply_to_snapshot(&mut snapshot),
            Err(GridDeltaApplyError::RegressingContentRevision {
                current: before.content_revision,
                received: malformed.content_revision,
            })
        );
        assert_eq!(snapshot, before);
        delta.apply_to_snapshot(&mut snapshot).unwrap();
        assert_eq!(snapshot, after);
        grid.process(b"\x1b7");
        let saved = grid.snapshot(0, 3);
        assert_eq!(saved.content_revision, after.content_revision);
        GridDeltaBatch::between(&after, &saved)
            .unwrap()
            .apply_to_snapshot(&mut snapshot)
            .unwrap();
        assert_eq!(snapshot, saved);
    }

    #[test]
    fn nonadvancing_revision_rejects_mutation_and_allows_valid_retry() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"main");
        let before = grid.snapshot(0, 3);
        grid.process(b"\x1b[?1049halt");
        let after = grid.snapshot(0, 3);
        let delta = GridDeltaBatch::between(&before, &after).unwrap();
        for revision in [0, before.revision - 1, before.revision] {
            let mut malformed = delta.clone();
            malformed.revision = revision;
            let mut snapshot = before.clone();
            assert_eq!(
                malformed.apply_to_snapshot(&mut snapshot),
                Err(GridDeltaApplyError::NonAdvancingRevision {
                    base: before.revision,
                    revision,
                })
            );
            assert_eq!(snapshot, before);
            delta.apply_to_snapshot(&mut snapshot).unwrap();
            assert_eq!(snapshot, after);
        }
    }

    #[test]
    fn malformed_replacement_indexes_preserve_snapshot_and_allow_retry() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"main");
        let before = grid.snapshot(0, 3);
        grid.process(b"\x1b[?1049hfirst\r\nsecond");
        let after = grid.snapshot(0, 3);
        let delta = GridDeltaBatch::between(&before, &after).unwrap();
        assert!(delta.reset_rows);
        for indexes in [[1, 0, 2], [0, 0, 2], [0, 2, 3], [0, 1, u32::MAX]] {
            let mut malformed = delta.clone();
            for (update, index) in malformed.row_updates.iter_mut().zip(indexes) {
                update.row_index = index;
            }
            let mut snapshot = before.clone();
            assert!(matches!(
                malformed.apply_to_snapshot(&mut snapshot),
                Err(GridDeltaApplyError::InvalidReplacementRowIndex { .. })
            ));
            assert_eq!(snapshot, before);
            delta.apply_to_snapshot(&mut snapshot).unwrap();
            assert_eq!(snapshot, after);
        }
    }

    #[test]
    fn sparse_row_indexes_must_be_unique_and_increasing() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        let before = grid.snapshot(0, 3);
        grid.process(b"first\r\nsecond");
        let after = grid.snapshot(0, 3);
        let valid = GridDeltaBatch::between(&before, &after).unwrap();
        assert!(!valid.reset_rows);
        assert_eq!(valid.row_updates.len(), 2);
        let mut duplicate = valid.clone();
        duplicate.row_updates[1].row_index = 0;
        let mut reversed = valid.clone();
        reversed.row_updates.reverse();
        for malformed in [duplicate, reversed] {
            let mut snapshot = before.clone();
            assert_eq!(
                malformed.apply_to_snapshot(&mut snapshot),
                Err(GridDeltaApplyError::NonIncreasingRowIndex(0))
            );
            assert_eq!(snapshot, before);
            valid.apply_to_snapshot(&mut snapshot).unwrap();
            assert_eq!(snapshot, after);
        }
    }

    #[test]
    fn invalid_screen_transitions_are_atomic_and_allow_retry() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"main");
        let before = grid.snapshot(0, 2);
        grid.process(b"\x1b[?1049halt");
        let after = grid.snapshot(0, 2);
        let valid = GridDeltaBatch::between(&before, &after).unwrap();
        let mut missing = valid.clone();
        missing.reset_rows = false;
        let mut width = valid.clone();
        width.width = 0;
        let mut height = valid.clone();
        height.height = 0;
        let mut mode = valid.clone();
        mode.mode = "unknown".to_owned();
        let mut resize = valid.clone();
        resize.mode.clone_from(&before.mode);
        resize.width += 1;
        resize.reset_rows = false;
        for (malformed, expected) in [
            (missing, GridDeltaApplyError::MissingReplacementRows),
            (width, GridDeltaApplyError::ZeroDimensions),
            (height, GridDeltaApplyError::ZeroDimensions),
            (mode, GridDeltaApplyError::InvalidScreenMode),
            (resize, GridDeltaApplyError::MissingReplacementRows),
        ] {
            let mut snapshot = before.clone();
            assert_eq!(malformed.apply_to_snapshot(&mut snapshot), Err(expected));
            assert_eq!(snapshot, before);
            valid.apply_to_snapshot(&mut snapshot).unwrap();
            assert_eq!(snapshot, after);
        }
    }

    #[test]
    fn malformed_delta_leaves_snapshot_unchanged_and_allows_retry() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"main\x1b[?1049halt");
        let before = grid.snapshot(0, 2);
        grid.process(b" changed");
        let after = grid.snapshot(0, 2);
        let delta = GridDeltaBatch::between(&before, &after).unwrap();
        assert!(!delta.reset_rows);
        assert!(!delta.row_updates.is_empty());
        let mut malformed = delta.clone();
        malformed.row_updates.push(RowUpdateSnapshot {
            row_index: u32::MAX,
            row: after.rows[0].clone(),
        });
        let mut snapshot = before.clone();
        assert_eq!(
            malformed.apply_to_snapshot(&mut snapshot),
            Err(GridDeltaApplyError::RowIndexOutOfBounds(u32::MAX))
        );
        assert_eq!(snapshot, before);
        delta.apply_to_snapshot(&mut snapshot).unwrap();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn duplicate_and_out_of_order_deltas_leave_snapshot_unchanged() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        let before = grid.snapshot(0, 2);
        grid.process(b"first");
        let middle = grid.snapshot(0, 2);
        let first = GridDeltaBatch::between(&before, &middle).unwrap();
        grid.process(b"\r\nsecond");
        let after = grid.snapshot(0, 2);
        let second = GridDeltaBatch::between(&middle, &after).unwrap();
        let mut snapshot = before.clone();
        assert!(matches!(
            second.apply_to_snapshot(&mut snapshot),
            Err(GridDeltaApplyError::RevisionMismatch { .. })
        ));
        assert_eq!(snapshot, before);
        first.apply_to_snapshot(&mut snapshot).unwrap();
        assert!(matches!(
            first.apply_to_snapshot(&mut snapshot),
            Err(GridDeltaApplyError::RevisionMismatch { .. })
        ));
        assert_eq!(snapshot, middle);
        second.apply_to_snapshot(&mut snapshot).unwrap();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn legacy_alternate_update_preserves_existing_backing_rows() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"main\x1b[?1049halt");
        let mut snapshot = grid.snapshot(0, 2);
        grid.process(b" update");
        let after = grid.snapshot(0, 2);
        let delta = GridDeltaBatch::between(&snapshot, &after).unwrap();
        let wire = serde_json::to_value(&delta).unwrap();
        assert!(wire.get("main_rows").is_none());
        let decoded: GridDeltaBatch = serde_json::from_value(wire).unwrap();
        assert!(decoded.main_rows.is_none());
        decoded.apply_to_snapshot(&mut snapshot).unwrap();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn alternate_entry_wire_delta_restores_main_screen_on_exit() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"main");
        let mut snapshot = grid.snapshot(0, 2);
        grid.process(b"\x1b[?1049halt");
        let after = grid.snapshot(0, 2);
        let delta = GridDeltaBatch::between(&snapshot, &after).unwrap();
        let decoded: GridDeltaBatch =
            serde_json::from_slice(&serde_json::to_vec(&delta).unwrap()).unwrap();
        assert_eq!(decoded, delta);
        decoded.apply_to_snapshot(&mut snapshot).unwrap();
        assert_eq!(snapshot, after);
        let mut hydrated = TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).unwrap();
        grid.process(b"\x1b[?1049l!");
        hydrated.process(b"\x1b[?1049l!");
        assert_eq!(hydrated.snapshot(0, 2), grid.snapshot(0, 2));
    }

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
