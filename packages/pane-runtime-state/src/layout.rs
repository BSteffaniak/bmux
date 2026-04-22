//! Layout tree + floating surface types.

use bmux_ipc::PaneSplitDirection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Integer rectangle describing a pane or floating surface bounds in
/// cell coordinates within an attach viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Floating surface anchored to a pane in a session. The surface is
/// rendered on top of the pane layout grid; it can be visible / opaque /
/// input-accepting / cursor-owning independently.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingSurfaceRuntime {
    pub id: Uuid,
    pub pane_id: Uuid,
    pub rect: LayoutRect,
    pub z: i32,
    pub visible: bool,
    pub opaque: bool,
    pub accepts_input: bool,
    pub cursor_owner: bool,
}

/// Layout tree for a session. Leaves are pane ids; splits carry a
/// direction + ratio (0.1..=0.9) and two child subtrees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneLayoutNode {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        direction: PaneSplitDirection,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl PaneLayoutNode {
    /// Append every leaf pane id into `out` in left-to-right traversal order.
    pub fn pane_order(&self, out: &mut Vec<Uuid>) {
        match self {
            Self::Leaf { pane_id } => out.push(*pane_id),
            Self::Split { first, second, .. } => {
                first.pane_order(out);
                second.pane_order(out);
            }
        }
    }

    /// Replace the leaf matching `target` with a new split
    /// `target-first / new_pane_id-second`. Returns `true` if a
    /// replacement happened.
    pub fn replace_leaf_with_split(
        &mut self,
        target: Uuid,
        direction: PaneSplitDirection,
        ratio: f32,
        new_pane_id: Uuid,
    ) -> bool {
        match self {
            Self::Leaf { pane_id } if *pane_id == target => {
                *self = Self::Split {
                    direction,
                    ratio,
                    first: Box::new(Self::Leaf { pane_id: target }),
                    second: Box::new(Self::Leaf {
                        pane_id: new_pane_id,
                    }),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.replace_leaf_with_split(target, direction, ratio, new_pane_id)
                    || second.replace_leaf_with_split(target, direction, ratio, new_pane_id)
            }
            Self::Leaf { .. } => false,
        }
    }

    /// Remove the leaf matching `target`; collapses enclosing split
    /// into its surviving sibling. Returns `true` if a removal happened.
    pub fn remove_leaf(&mut self, target: Uuid) -> bool {
        enum RemoveResult {
            NotFound,
            Removed,
            RemoveThis,
        }

        fn remove_inner(node: &mut PaneLayoutNode, target: Uuid) -> RemoveResult {
            match node {
                PaneLayoutNode::Leaf { pane_id } => {
                    if *pane_id == target {
                        RemoveResult::RemoveThis
                    } else {
                        RemoveResult::NotFound
                    }
                }
                PaneLayoutNode::Split { first, second, .. } => {
                    match remove_inner(first, target) {
                        RemoveResult::NotFound => {}
                        RemoveResult::Removed => return RemoveResult::Removed,
                        RemoveResult::RemoveThis => {
                            *node = (**second).clone();
                            return RemoveResult::Removed;
                        }
                    }

                    match remove_inner(second, target) {
                        RemoveResult::NotFound => RemoveResult::NotFound,
                        RemoveResult::Removed => RemoveResult::Removed,
                        RemoveResult::RemoveThis => {
                            *node = (**first).clone();
                            RemoveResult::Removed
                        }
                    }
                }
            }
        }

        !matches!(remove_inner(self, target), RemoveResult::NotFound)
    }

    /// Find the deepest split containing `target` and adjust its
    /// ratio by `delta`, clamped to `[0.1, 0.9]`. Returns the new
    /// ratio, or `None` if no containing split was found.
    pub fn adjust_focused_ratio(&mut self, target: Uuid, delta: f32) -> Option<f32> {
        match self {
            Self::Leaf { .. } => None,
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if contains_pane(first, target) || contains_pane(second, target) {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    Some(*ratio)
                } else {
                    first
                        .adjust_focused_ratio(target, delta)
                        .or_else(|| second.adjust_focused_ratio(target, delta))
                }
            }
        }
    }
}

/// Recursive membership check — true if `pane_id` appears as a leaf
/// anywhere in the subtree rooted at `node`.
#[must_use]
pub fn contains_pane(node: &PaneLayoutNode, pane_id: Uuid) -> bool {
    match node {
        PaneLayoutNode::Leaf { pane_id: id } => *id == pane_id,
        PaneLayoutNode::Split { first, second, .. } => {
            contains_pane(first, pane_id) || contains_pane(second, pane_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutRect, PaneLayoutNode, contains_pane};
    use bmux_ipc::PaneSplitDirection;
    use uuid::Uuid;

    fn leaf(id: Uuid) -> PaneLayoutNode {
        PaneLayoutNode::Leaf { pane_id: id }
    }

    #[test]
    fn pane_order_walks_left_to_right() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(PaneLayoutNode::Split {
                direction: PaneSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(leaf(b)),
                second: Box::new(leaf(c)),
            }),
        };
        let mut out = Vec::new();
        root.pane_order(&mut out);
        assert_eq!(out, vec![a, b, c]);
    }

    #[test]
    fn replace_leaf_with_split_descends_into_subtree() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let new_id = Uuid::new_v4();
        let mut root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(leaf(b)),
        };
        assert!(root.replace_leaf_with_split(b, PaneSplitDirection::Vertical, 0.5, new_id,));
        let mut out = Vec::new();
        root.pane_order(&mut out);
        assert_eq!(out, vec![a, b, new_id]);
    }

    #[test]
    fn remove_leaf_collapses_sibling_into_parent() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(leaf(b)),
        };
        assert!(root.remove_leaf(a));
        match root {
            PaneLayoutNode::Leaf { pane_id } => assert_eq!(pane_id, b),
            PaneLayoutNode::Split { .. } => panic!("expected collapsed leaf"),
        }
    }

    #[test]
    fn adjust_focused_ratio_clamps_to_range() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(leaf(b)),
        };
        assert_eq!(root.adjust_focused_ratio(a, 1.0), Some(0.9));
        assert_eq!(root.adjust_focused_ratio(a, -5.0), Some(0.1));
    }

    #[test]
    fn contains_pane_searches_recursively() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.3,
            first: Box::new(leaf(a)),
            second: Box::new(leaf(b)),
        };
        assert!(contains_pane(&root, a));
        assert!(contains_pane(&root, b));
        assert!(!contains_pane(&root, Uuid::new_v4()));
    }

    #[test]
    fn layout_rect_round_trips_through_json() {
        let rect = LayoutRect {
            x: 10,
            y: 20,
            w: 80,
            h: 24,
        };
        let bytes = serde_json::to_vec(&rect).unwrap();
        let decoded: LayoutRect = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(rect, decoded);
    }
}
