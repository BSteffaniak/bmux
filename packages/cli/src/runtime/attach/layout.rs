#![allow(dead_code)]

use super::state::PaneRect;
use bmux_ipc::{PaneLayoutNode, PaneSplitDirection};
use std::collections::BTreeMap;
use uuid::Uuid;

pub fn collect_pane_ids(layout: &PaneLayoutNode, out: &mut Vec<Uuid>) {
    match layout {
        PaneLayoutNode::Leaf { pane_id } => out.push(*pane_id),
        PaneLayoutNode::Split { first, second, .. } => {
            collect_pane_ids(first, out);
            collect_pane_ids(second, out);
        }
    }
}

fn split_rect(rect: PaneRect, ratio_percent: u8, vertical: bool) -> (PaneRect, PaneRect) {
    if vertical {
        let split = ((u32::from(rect.w) * u32::from(ratio_percent)) / 100) as u16;
        let left_w = split.max(1).min(rect.w.saturating_sub(1));
        let right_w = rect.w.saturating_sub(left_w);
        (
            PaneRect {
                x: rect.x,
                y: rect.y,
                w: left_w,
                h: rect.h,
            },
            PaneRect {
                x: rect.x.saturating_add(left_w),
                y: rect.y,
                w: right_w,
                h: rect.h,
            },
        )
    } else {
        let split = ((u32::from(rect.h) * u32::from(ratio_percent)) / 100) as u16;
        let top_h = split.max(1).min(rect.h.saturating_sub(1));
        let bottom_h = rect.h.saturating_sub(top_h);
        (
            PaneRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: top_h,
            },
            PaneRect {
                x: rect.x,
                y: rect.y.saturating_add(top_h),
                w: rect.w,
                h: bottom_h,
            },
        )
    }
}

pub fn collect_layout_rects(
    layout: &PaneLayoutNode,
    rect: PaneRect,
    out: &mut BTreeMap<Uuid, PaneRect>,
) {
    match layout {
        PaneLayoutNode::Leaf { pane_id } => {
            out.insert(*pane_id, rect);
        }
        PaneLayoutNode::Split {
            direction,
            ratio_percent,
            first,
            second,
        } => {
            let vertical = matches!(direction, PaneSplitDirection::Vertical);
            let (first_rect, second_rect) = split_rect(rect, *ratio_percent, vertical);
            collect_layout_rects(first, first_rect, out);
            collect_layout_rects(second, second_rect, out);
        }
    }
}
