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
pub(crate) struct Layout {
    pub(crate) left: Rect,
    pub(crate) right: Rect,
    pub(crate) divider_x: u16,
}

pub(crate) fn compute_vertical_layout(cols: u16, rows: u16, ratio: f32) -> Layout {
    let body_rows = rows.saturating_sub(1);
    let usable_cols = cols.max(3);

    let split_cols = usable_cols.saturating_sub(1);
    let clamped_ratio = ratio.clamp(0.2, 0.8);

    let mut left_width = ((f32::from(split_cols) * clamped_ratio).round()) as u16;
    left_width = left_width.clamp(1, split_cols.saturating_sub(1));
    let right_width = split_cols.saturating_sub(left_width);

    let left = Rect {
        x: 1,
        y: 2,
        width: left_width,
        height: body_rows,
    };

    let right = Rect {
        x: left_width.saturating_add(2),
        y: 2,
        width: right_width,
        height: body_rows,
    };

    Layout {
        left,
        right,
        divider_x: left_width.saturating_add(1),
    }
}

#[cfg(test)]
mod tests {
    use super::compute_vertical_layout;

    #[test]
    fn produces_two_non_zero_panes() {
        let layout = compute_vertical_layout(120, 40, 0.5);
        assert!(layout.left.width > 0);
        assert!(layout.right.width > 0);
        assert_eq!(layout.left.height, 39);
        assert_eq!(layout.right.height, 39);
    }

    #[test]
    fn respects_ratio_clamp() {
        let layout = compute_vertical_layout(100, 30, 0.95);
        assert!(layout.left.width < 90);
    }
}
