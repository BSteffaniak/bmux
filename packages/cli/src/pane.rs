use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PaneId(pub(crate) u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Rect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

impl Rect {
    pub(crate) fn inner(self) -> Rect {
        Rect {
            x: self.x.saturating_add(1),
            y: self.y.saturating_add(1),
            width: self.width.saturating_sub(2),
            height: self.height.saturating_sub(2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LayoutNode {
    Leaf {
        pane_id: PaneId,
    },
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LayoutTree {
    pub(crate) root: LayoutNode,
    pub(crate) focused: PaneId,
}

impl LayoutTree {
    pub(crate) fn two_pane(
        first: PaneId,
        second: PaneId,
        direction: SplitDirection,
        ratio: f32,
    ) -> Self {
        Self {
            root: LayoutNode::Split {
                direction,
                ratio,
                first: Box::new(LayoutNode::Leaf { pane_id: first }),
                second: Box::new(LayoutNode::Leaf { pane_id: second }),
            },
            focused: first,
        }
    }

    pub(crate) fn toggle_focused_split_direction(&mut self) -> Option<SplitDirection> {
        let mut changed = None;
        toggle_nearest_split_direction(&mut self.root, self.focused, &mut changed);
        changed
    }

    pub(crate) fn adjust_focused_split_ratio(&mut self, delta: f32) -> Option<f32> {
        let mut updated = None;
        adjust_nearest_split_ratio(&mut self.root, self.focused, delta, &mut updated);
        updated
    }

    pub(crate) fn adjust_focused_split_toward(
        &mut self,
        direction: ResizeDirection,
        step: f32,
    ) -> Option<f32> {
        let mut updated = None;
        adjust_nearest_split_toward(&mut self.root, self.focused, direction, step, &mut updated);
        updated
    }

    pub(crate) fn pane_order(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        collect_pane_ids(&self.root, &mut out);
        out
    }

    pub(crate) fn compute_rects(&self, cols: u16, rows: u16) -> BTreeMap<PaneId, Rect> {
        let mut rects = BTreeMap::new();
        let body_rows = rows.saturating_sub(1);
        let root_rect = Rect {
            x: 1,
            y: 2,
            width: cols.max(3),
            height: body_rows.max(3),
        };
        compute_node_rects(&self.root, root_rect, &mut rects);
        rects
    }

    pub(crate) fn split_focused(
        &mut self,
        direction: SplitDirection,
        new_pane_id: PaneId,
        ratio: f32,
    ) -> bool {
        let focused = self.focused;
        let replacement = LayoutNode::Split {
            direction,
            ratio: ratio.clamp(0.2, 0.8),
            first: Box::new(LayoutNode::Leaf { pane_id: focused }),
            second: Box::new(LayoutNode::Leaf {
                pane_id: new_pane_id,
            }),
        };

        if replace_leaf(&mut self.root, focused, replacement) {
            self.focused = new_pane_id;
            true
        } else {
            false
        }
    }

    pub(crate) fn remove_pane(&mut self, pane_id: PaneId) -> bool {
        let removed = remove_leaf(&mut self.root, pane_id);
        if !removed {
            return false;
        }

        if self.focused == pane_id {
            if let Some(next_focus) = self.pane_order().first().copied() {
                self.focused = next_focus;
            }
        }

        true
    }
}

fn collect_pane_ids(node: &LayoutNode, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Leaf { pane_id } => out.push(*pane_id),
        LayoutNode::Split { first, second, .. } => {
            collect_pane_ids(first, out);
            collect_pane_ids(second, out);
        }
    }
}

fn replace_leaf(node: &mut LayoutNode, target: PaneId, replacement: LayoutNode) -> bool {
    match node {
        LayoutNode::Leaf { pane_id } => {
            if *pane_id == target {
                *node = replacement;
                true
            } else {
                false
            }
        }
        LayoutNode::Split { first, second, .. } => {
            replace_leaf(first, target, replacement.clone())
                || replace_leaf(second, target, replacement)
        }
    }
}

fn toggle_nearest_split_direction(
    node: &mut LayoutNode,
    target: PaneId,
    changed: &mut Option<SplitDirection>,
) -> bool {
    match node {
        LayoutNode::Leaf { pane_id } => *pane_id == target,
        LayoutNode::Split {
            direction,
            first,
            second,
            ..
        } => {
            let contains = toggle_nearest_split_direction(first, target, changed)
                || toggle_nearest_split_direction(second, target, changed);
            if contains && changed.is_none() {
                *direction = match *direction {
                    SplitDirection::Vertical => SplitDirection::Horizontal,
                    SplitDirection::Horizontal => SplitDirection::Vertical,
                };
                *changed = Some(*direction);
            }
            contains
        }
    }
}

fn adjust_nearest_split_ratio(
    node: &mut LayoutNode,
    target: PaneId,
    delta: f32,
    updated: &mut Option<f32>,
) -> bool {
    match node {
        LayoutNode::Leaf { pane_id } => *pane_id == target,
        LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            let contains = adjust_nearest_split_ratio(first, target, delta, updated)
                || adjust_nearest_split_ratio(second, target, delta, updated);
            if contains && updated.is_none() {
                *ratio = (*ratio + delta).clamp(0.2, 0.8);
                *updated = Some(*ratio);
            }
            contains
        }
    }
}

fn adjust_nearest_split_toward(
    node: &mut LayoutNode,
    target: PaneId,
    direction: ResizeDirection,
    step: f32,
    updated: &mut Option<f32>,
) -> bool {
    match node {
        LayoutNode::Leaf { pane_id } => *pane_id == target,
        LayoutNode::Split {
            direction: split_direction,
            ratio,
            first,
            second,
        } => {
            let in_first = adjust_nearest_split_toward(first, target, direction, step, updated);
            let in_second = if in_first {
                false
            } else {
                adjust_nearest_split_toward(second, target, direction, step, updated)
            };
            let contains = in_first || in_second;
            if contains && updated.is_none() {
                let delta = match (*split_direction, direction) {
                    (SplitDirection::Vertical, ResizeDirection::Left) => {
                        if in_first {
                            step
                        } else {
                            -step
                        }
                    }
                    (SplitDirection::Vertical, ResizeDirection::Right) => {
                        if in_first {
                            -step
                        } else {
                            step
                        }
                    }
                    (SplitDirection::Horizontal, ResizeDirection::Up) => {
                        if in_first {
                            step
                        } else {
                            -step
                        }
                    }
                    (SplitDirection::Horizontal, ResizeDirection::Down) => {
                        if in_first {
                            -step
                        } else {
                            step
                        }
                    }
                    _ => 0.0,
                };

                if delta != 0.0 {
                    *ratio = (*ratio + delta).clamp(0.2, 0.8);
                    *updated = Some(*ratio);
                }
            }
            contains
        }
    }
}

fn remove_leaf(node: &mut LayoutNode, target: PaneId) -> bool {
    match node {
        LayoutNode::Leaf { pane_id } => *pane_id == target,
        LayoutNode::Split { first, second, .. } => match (&**first, &**second) {
            (LayoutNode::Leaf { pane_id }, _) if *pane_id == target => {
                *node = (*second.clone()).clone();
                true
            }
            (_, LayoutNode::Leaf { pane_id }) if *pane_id == target => {
                *node = (*first.clone()).clone();
                true
            }
            _ => {
                if remove_leaf(first, target) {
                    true
                } else {
                    remove_leaf(second, target)
                }
            }
        },
    }
}

fn compute_node_rects(node: &LayoutNode, rect: Rect, out: &mut BTreeMap<PaneId, Rect>) {
    match node {
        LayoutNode::Leaf { pane_id } => {
            out.insert(*pane_id, rect);
        }
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => match direction {
            SplitDirection::Vertical => {
                let split_cols = rect.width.saturating_sub(1);
                let mut first_width =
                    ((f32::from(split_cols) * ratio.clamp(0.2, 0.8)).round()) as u16;
                first_width = first_width.clamp(1, split_cols.saturating_sub(1));
                let second_width = split_cols.saturating_sub(first_width);

                let first_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: first_width,
                    height: rect.height,
                };
                let second_rect = Rect {
                    x: rect.x.saturating_add(first_width).saturating_add(1),
                    y: rect.y,
                    width: second_width,
                    height: rect.height,
                };

                compute_node_rects(first, first_rect, out);
                compute_node_rects(second, second_rect, out);
            }
            SplitDirection::Horizontal => {
                let split_rows = rect.height;
                let mut first_height =
                    ((f32::from(split_rows) * ratio.clamp(0.2, 0.8)).round()) as u16;
                first_height = first_height.clamp(1, split_rows.saturating_sub(1));
                let second_height = split_rows.saturating_sub(first_height);

                let first_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: first_height,
                };
                let second_rect = Rect {
                    x: rect.x,
                    y: rect.y.saturating_add(first_height),
                    width: rect.width,
                    height: second_height,
                };

                compute_node_rects(first, first_rect, out);
                compute_node_rects(second, second_rect, out);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutTree, PaneId, ResizeDirection, SplitDirection};

    #[test]
    fn computes_two_pane_vertical_rects() {
        let tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let rects = tree.compute_rects(120, 40);
        assert_eq!(rects[&PaneId(1)].y, 2);
        assert_eq!(rects[&PaneId(2)].y, 2);
    }

    #[test]
    fn computes_two_pane_horizontal_rects() {
        let tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Horizontal, 0.5);
        let rects = tree.compute_rects(120, 40);
        let top = rects[&PaneId(1)];
        let bottom = rects[&PaneId(2)];
        assert_eq!(bottom.y, top.y + top.height);
    }

    #[test]
    fn split_focused_adds_new_leaf_and_focuses_it() {
        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        tree.focused = PaneId(1);
        let added = tree.split_focused(SplitDirection::Horizontal, PaneId(3), 0.5);
        assert!(added);
        let order = tree.pane_order();
        assert!(order.contains(&PaneId(3)));
        assert_eq!(tree.focused, PaneId(3));
    }

    #[test]
    fn remove_pane_rebalances_tree() {
        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let removed = tree.remove_pane(PaneId(2));
        assert!(removed);
        assert_eq!(tree.pane_order(), vec![PaneId(1)]);
    }

    #[test]
    fn toggle_updates_nearest_focused_split_only() {
        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        tree.split_focused(SplitDirection::Horizontal, PaneId(3), 0.5);
        tree.focused = PaneId(1);

        let changed = tree.toggle_focused_split_direction();
        assert_eq!(changed, Some(SplitDirection::Vertical));
    }

    #[test]
    fn resize_updates_nearest_focused_split_ratio() {
        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        tree.split_focused(SplitDirection::Horizontal, PaneId(3), 0.5);
        tree.focused = PaneId(1);

        let updated = tree.adjust_focused_split_ratio(0.1);
        assert_eq!(updated, Some(0.6));
    }

    #[test]
    fn directional_resize_updates_matching_axis_split() {
        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        tree.focused = PaneId(2);
        assert!(tree.split_focused(SplitDirection::Horizontal, PaneId(3), 0.5));
        tree.focused = PaneId(3);

        let updated = tree.adjust_focused_split_toward(ResizeDirection::Up, 0.05);
        assert_eq!(updated, Some(0.45));
    }

    #[test]
    fn directional_resize_noops_when_axis_missing() {
        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        tree.focused = PaneId(1);

        let updated = tree.adjust_focused_split_toward(ResizeDirection::Up, 0.05);
        assert_eq!(updated, None);
    }
}
