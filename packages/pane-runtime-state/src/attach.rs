//! Attach-viewport record.

use serde::{Deserialize, Serialize};

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
    pub top_inset: u16,
    pub right_inset: u16,
    pub bottom_inset: u16,
    pub left_inset: u16,
}

#[cfg(test)]
mod tests {
    use super::AttachViewport;

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
        let bytes = serde_json::to_vec(&viewport).unwrap();
        let decoded: AttachViewport = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(viewport, decoded);
    }
}
