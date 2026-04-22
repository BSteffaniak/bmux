//! Attach-viewport record.

use serde::{Deserialize, Serialize};

/// Per-attach viewport dimensions — what the attaching client has
/// rendered space for. The plugin uses these to compute per-pane
/// `LayoutRect`s (splitting the viewport along the layout tree) and
/// to resize underlying PTYs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachViewport {
    pub cols: u16,
    pub rows: u16,
    pub status_top_inset: u16,
    pub status_bottom_inset: u16,
}

#[cfg(test)]
mod tests {
    use super::AttachViewport;

    #[test]
    fn viewport_round_trips_through_json() {
        let viewport = AttachViewport {
            cols: 120,
            rows: 40,
            status_top_inset: 2,
            status_bottom_inset: 1,
        };
        let bytes = serde_json::to_vec(&viewport).unwrap();
        let decoded: AttachViewport = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(viewport, decoded);
    }
}
