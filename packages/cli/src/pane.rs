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

    pub(crate) fn set_direction(&mut self, direction: SplitDirection) {
        if let LayoutNode::Split {
            direction: root_direction,
            ..
        } = &mut self.root
        {
            *root_direction = direction;
        }
    }

    pub(crate) fn direction(&self) -> SplitDirection {
        if let LayoutNode::Split { direction, .. } = &self.root {
            *direction
        } else {
            SplitDirection::Vertical
        }
    }

    pub(crate) fn set_ratio(&mut self, ratio: f32) {
        if let LayoutNode::Split {
            ratio: root_ratio, ..
        } = &mut self.root
        {
            *root_ratio = ratio.clamp(0.2, 0.8);
        }
    }

    pub(crate) fn ratio(&self) -> f32 {
        if let LayoutNode::Split { ratio, .. } = &self.root {
            *ratio
        } else {
            0.5
        }
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
    use super::{LayoutTree, PaneId, SplitDirection};

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
}
