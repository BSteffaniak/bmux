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

/// Directional intent for resizing a focused pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneResizeDirection {
    Increase,
    Decrease,
    Left,
    Right,
    Up,
    Down,
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

    /// Resize `target` by moving the nearest relevant split boundary.
    ///
    /// `Increase` / `Decrease` operate on the deepest containing split,
    /// growing or shrinking the target pane. Physical directions walk
    /// outward until they find the nearest boundary on that side.
    pub fn resize_focused(
        &mut self,
        target: Uuid,
        direction: PaneResizeDirection,
        rect: LayoutRect,
        cells: u16,
    ) -> Option<f32> {
        match direction {
            PaneResizeDirection::Increase | PaneResizeDirection::Decrease => {
                self.resize_focused_automatic(target, direction, rect, cells)
            }
            PaneResizeDirection::Left
            | PaneResizeDirection::Right
            | PaneResizeDirection::Up
            | PaneResizeDirection::Down => {
                self.resize_focused_directional(target, direction, rect, cells)
            }
        }
    }

    fn resize_focused_automatic(
        &mut self,
        target: Uuid,
        direction: PaneResizeDirection,
        rect: LayoutRect,
        cells: u16,
    ) -> Option<f32> {
        match self {
            Self::Leaf { .. } => None,
            Self::Split {
                direction: split_direction,
                ratio,
                first,
                second,
            } => {
                let vertical = matches!(split_direction, PaneSplitDirection::Vertical);
                let (first_rect, second_rect) = split_layout_rect(rect, *ratio, vertical);
                if contains_pane(first, target) {
                    if let Some(adjusted) =
                        first.resize_focused_automatic(target, direction, first_rect, cells)
                    {
                        return Some(adjusted);
                    }
                    let delta = ratio_delta(rect, vertical, cells);
                    let signed_delta = if matches!(direction, PaneResizeDirection::Increase) {
                        delta
                    } else {
                        -delta
                    };
                    *ratio = (*ratio + signed_delta).clamp(0.1, 0.9);
                    Some(*ratio)
                } else if contains_pane(second, target) {
                    if let Some(adjusted) =
                        second.resize_focused_automatic(target, direction, second_rect, cells)
                    {
                        return Some(adjusted);
                    }
                    let delta = ratio_delta(rect, vertical, cells);
                    let signed_delta = if matches!(direction, PaneResizeDirection::Increase) {
                        -delta
                    } else {
                        delta
                    };
                    *ratio = (*ratio + signed_delta).clamp(0.1, 0.9);
                    Some(*ratio)
                } else {
                    None
                }
            }
        }
    }

    fn resize_focused_directional(
        &mut self,
        target: Uuid,
        direction: PaneResizeDirection,
        rect: LayoutRect,
        cells: u16,
    ) -> Option<f32> {
        match self {
            Self::Leaf { .. } => None,
            Self::Split {
                direction: split_direction,
                ratio,
                first,
                second,
            } => {
                let vertical = matches!(split_direction, PaneSplitDirection::Vertical);
                let (first_rect, second_rect) = split_layout_rect(rect, *ratio, vertical);
                if contains_pane(first, target) {
                    if let Some(adjusted) =
                        first.resize_focused_directional(target, direction, first_rect, cells)
                    {
                        return Some(adjusted);
                    }
                    if vertical && matches!(direction, PaneResizeDirection::Right)
                        || !vertical && matches!(direction, PaneResizeDirection::Down)
                    {
                        *ratio = (*ratio + ratio_delta(rect, vertical, cells)).clamp(0.1, 0.9);
                        return Some(*ratio);
                    }
                } else if contains_pane(second, target) {
                    if let Some(adjusted) =
                        second.resize_focused_directional(target, direction, second_rect, cells)
                    {
                        return Some(adjusted);
                    }
                    if vertical && matches!(direction, PaneResizeDirection::Left)
                        || !vertical && matches!(direction, PaneResizeDirection::Up)
                    {
                        *ratio = (*ratio - ratio_delta(rect, vertical, cells)).clamp(0.1, 0.9);
                        return Some(*ratio);
                    }
                }
                None
            }
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio_delta(rect: LayoutRect, vertical: bool, cells: u16) -> f32 {
    let span = if vertical { rect.w } else { rect.h }.max(1);
    f32::from(cells) / f32::from(span)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn split_layout_rect(rect: LayoutRect, ratio: f32, vertical: bool) -> (LayoutRect, LayoutRect) {
    let ratio = ratio.clamp(0.1, 0.9);
    if vertical {
        let split = ((f32::from(rect.w) * ratio).round()) as u16;
        let first_w = split.max(1).min(rect.w.saturating_sub(1).max(1));
        let second_w = rect.w.saturating_sub(first_w).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: first_w,
                h: rect.h,
            },
            LayoutRect {
                x: rect.x.saturating_add(first_w),
                y: rect.y,
                w: second_w,
                h: rect.h,
            },
        )
    } else {
        let split = ((f32::from(rect.h) * ratio).round()) as u16;
        let first_h = split.max(1).min(rect.h.saturating_sub(1).max(1));
        let second_h = rect.h.saturating_sub(first_h).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: first_h,
            },
            LayoutRect {
                x: rect.x,
                y: rect.y.saturating_add(first_h),
                w: rect.w,
                h: second_h,
            },
        )
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
    use super::{LayoutRect, PaneLayoutNode, PaneResizeDirection, contains_pane};
    use bmux_ipc::PaneSplitDirection;
    use uuid::Uuid;

    fn leaf(id: Uuid) -> PaneLayoutNode {
        PaneLayoutNode::Leaf { pane_id: id }
    }

    fn assert_ratio(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < f32::EPSILON);
    }

    fn assert_resize_ratio(actual: Option<f32>, expected: f32) {
        let actual = actual.expect("resize should adjust a split");
        assert_ratio(actual, expected);
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
    fn resize_focused_clamps_to_range() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(leaf(b)),
        };
        let rect = LayoutRect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        assert_resize_ratio(
            root.resize_focused(a, PaneResizeDirection::Increase, rect, 100),
            0.9,
        );
        assert_resize_ratio(
            root.resize_focused(a, PaneResizeDirection::Decrease, rect, 500),
            0.1,
        );
    }

    #[test]
    fn resize_focused_automatic_uses_deepest_split() {
        let left = Uuid::new_v4();
        let center = Uuid::new_v4();
        let right = Uuid::new_v4();
        let mut root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.25,
            first: Box::new(leaf(left)),
            second: Box::new(PaneLayoutNode::Split {
                direction: PaneSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(leaf(center)),
                second: Box::new(leaf(right)),
            }),
        };
        let rect = LayoutRect {
            x: 0,
            y: 0,
            w: 100,
            h: 40,
        };

        assert_resize_ratio(
            root.resize_focused(center, PaneResizeDirection::Increase, rect, 10),
            0.633_333_3,
        );
        match root {
            PaneLayoutNode::Split { ratio, second, .. } => {
                assert_ratio(ratio, 0.25);
                match *second {
                    PaneLayoutNode::Split { ratio, .. } => assert_ratio(ratio, 0.633_333_3),
                    PaneLayoutNode::Leaf { .. } => panic!("expected nested split"),
                }
            }
            PaneLayoutNode::Leaf { .. } => panic!("expected root split"),
        }
    }

    #[test]
    fn resize_focused_grows_second_child_on_increase() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(leaf(b)),
        };
        let rect = LayoutRect {
            x: 0,
            y: 0,
            w: 100,
            h: 40,
        };
        assert_resize_ratio(
            root.resize_focused(b, PaneResizeDirection::Increase, rect, 10),
            0.4,
        );
    }

    #[test]
    fn resize_focused_directional_uses_nearest_boundary() {
        let left = Uuid::new_v4();
        let center = Uuid::new_v4();
        let right = Uuid::new_v4();
        let mut root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.25,
            first: Box::new(leaf(left)),
            second: Box::new(PaneLayoutNode::Split {
                direction: PaneSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(leaf(center)),
                second: Box::new(leaf(right)),
            }),
        };
        let rect = LayoutRect {
            x: 0,
            y: 0,
            w: 100,
            h: 40,
        };
        assert_resize_ratio(
            root.resize_focused(center, PaneResizeDirection::Right, rect, 10),
            0.633_333_3,
        );
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
