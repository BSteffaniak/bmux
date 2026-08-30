//! Attach-viewport record.

use serde::{Deserialize, Serialize};

/// Resolved pane content region inside an attach viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachContentRegion {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

/// Per-attach viewport dimensions and generic reserved edge insets.
///
/// The plugin uses the resulting content region to compute per-pane
/// `LayoutRect`s and resize underlying PTYs. Insets are presentation-neutral:
/// attach clients may reserve viewport cells for any independently composed
/// surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachViewport {
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub top_inset: u16,
    #[serde(default)]
    pub right_inset: u16,
    #[serde(default)]
    pub bottom_inset: u16,
    #[serde(default)]
    pub left_inset: u16,
}

impl AttachViewport {
    /// Normalize an already-decoded viewport, including persisted snapshots.
    #[must_use]
    pub fn normalize(self) -> Self {
        Self::normalized(
            self.cols,
            self.rows,
            self.top_inset,
            self.right_inset,
            self.bottom_inset,
            self.left_inset,
        )
    }

    /// Construct a viewport with dimensions and insets normalized to preserve
    /// at least one content cell on each axis.
    #[must_use]
    pub fn normalized(
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
    ) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(2);
        let (top_inset, bottom_inset) = normalize_axis_insets(rows, top_inset, bottom_inset);
        let (left_inset, right_inset) = normalize_axis_insets(cols, left_inset, right_inset);
        Self {
            cols,
            rows,
            top_inset,
            right_inset,
            bottom_inset,
            left_inset,
        }
    }

    /// Resolve the authoritative non-empty content region used for pane layout.
    #[must_use]
    pub fn content_region(self) -> AttachContentRegion {
        let x = self.left_inset.min(self.cols.saturating_sub(1));
        let y = self.top_inset.min(self.rows.saturating_sub(1));
        let reserved_cols = self.left_inset.saturating_add(self.right_inset);
        let reserved_rows = self.top_inset.saturating_add(self.bottom_inset);
        AttachContentRegion {
            x,
            y,
            cols: self.cols.saturating_sub(reserved_cols).max(1),
            rows: self.rows.saturating_sub(reserved_rows).max(1),
        }
    }
}

fn normalize_axis_insets(extent: u16, leading: u16, trailing: u16) -> (u16, u16) {
    let maximum = extent.saturating_sub(1);
    let leading = leading.min(maximum);
    let trailing = trailing.min(maximum.saturating_sub(leading));
    (leading, trailing)
}

#[cfg(test)]
mod tests {
    use super::AttachViewport;

    #[test]
    fn viewport_normalization_preserves_requested_leading_edges_and_one_cell() {
        let viewport = AttachViewport::normalized(10, 5, 4, 9, 4, 9);
        assert_eq!(
            viewport,
            AttachViewport {
                cols: 10,
                rows: 5,
                top_inset: 4,
                right_inset: 0,
                bottom_inset: 0,
                left_inset: 9,
            }
        );
        assert_eq!(viewport.content_region().cols, 1);
        assert_eq!(viewport.content_region().rows, 1);
    }

    #[test]
    fn viewport_resolves_clamped_nonempty_content_region() {
        let viewport = AttachViewport {
            cols: 10,
            rows: 5,
            top_inset: 2,
            right_inset: 20,
            bottom_inset: 20,
            left_inset: 3,
        };
        assert_eq!(
            viewport.content_region(),
            super::AttachContentRegion {
                x: 3,
                y: 2,
                cols: 1,
                rows: 1,
            }
        );

        let empty = AttachViewport {
            cols: 0,
            rows: 0,
            top_inset: 1,
            right_inset: 1,
            bottom_inset: 1,
            left_inset: 1,
        };
        assert_eq!(
            empty.content_region(),
            super::AttachContentRegion {
                x: 0,
                y: 0,
                cols: 1,
                rows: 1,
            }
        );
    }

    #[test]
    fn viewport_decodes_missing_insets_as_zero() {
        let decoded: AttachViewport =
            serde_json::from_value(serde_json::json!({ "cols": 120, "rows": 40 })).unwrap();
        assert_eq!(
            decoded,
            AttachViewport {
                cols: 120,
                rows: 40,
                top_inset: 0,
                right_inset: 0,
                bottom_inset: 0,
                left_inset: 0,
            }
        );
    }

    #[test]
    fn viewport_round_trips_through_json() {
        let viewport = AttachViewport {
            cols: 120,
            rows: 40,
            top_inset: 2,
            right_inset: 4,
            bottom_inset: 1,
            left_inset: 3,
        };
        assert_eq!(viewport.normalize(), viewport);
        let bytes = serde_json::to_vec(&viewport).unwrap();
        let decoded: AttachViewport = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(viewport, decoded);
    }
}
