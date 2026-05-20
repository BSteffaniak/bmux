//! Deterministic terminal layout helpers.

use crate::geometry::{Insets, Rect, Size};

/// Layout direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Split from left to right.
    #[default]
    Horizontal,
    /// Split from top to bottom.
    Vertical,
}

/// A layout segment constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// Fixed cell count.
    Length(u16),
    /// Percentage of the available axis, clamped to `0..=100`.
    Percentage(u16),
    /// Proportional share of remaining space.
    Ratio(u16),
    /// At least the supplied cell count, plus a proportional share of remaining space.
    Min(u16),
    /// Fill remaining space with equal weight.
    Fill,
}

/// Width thresholds for responsive terminal layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakpoints {
    /// Width below this value is compact.
    pub compact_max_exclusive: u16,
    /// Width below this value is medium; wider is wide.
    pub wide_min: u16,
}

impl Default for Breakpoints {
    fn default() -> Self {
        Self {
            compact_max_exclusive: 80,
            wide_min: 120,
        }
    }
}

impl Breakpoints {
    /// Classify a terminal size by configured width thresholds.
    #[must_use]
    pub const fn classify(self, size: Size) -> Breakpoint {
        if size.width < self.compact_max_exclusive {
            Breakpoint::Compact
        } else if size.width < self.wide_min {
            Breakpoint::Medium
        } else {
            Breakpoint::Wide
        }
    }
}

/// Values selected by responsive breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Responsive<T> {
    /// Compact value.
    pub compact: T,
    /// Medium value.
    pub medium: T,
    /// Wide value.
    pub wide: T,
}

impl<T: Copy> Responsive<T> {
    /// Create responsive values.
    #[must_use]
    pub const fn new(compact: T, medium: T, wide: T) -> Self {
        Self {
            compact,
            medium,
            wide,
        }
    }

    /// Return the value for a breakpoint.
    #[must_use]
    pub const fn for_breakpoint(self, breakpoint: Breakpoint) -> T {
        match breakpoint {
            Breakpoint::Compact => self.compact,
            Breakpoint::Medium => self.medium,
            Breakpoint::Wide => self.wide,
        }
    }

    /// Return the value for a terminal size using default breakpoints.
    #[must_use]
    pub const fn for_size(self, size: Size) -> T {
        self.for_breakpoint(Breakpoint::for_size(size))
    }
}

/// Responsive terminal width class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Breakpoint {
    /// Narrow/compact terminal width.
    Compact,
    /// Medium terminal width.
    Medium,
    /// Wide terminal width.
    Wide,
}

impl Breakpoint {
    /// Classify a terminal size by width.
    #[must_use]
    pub const fn for_size(size: Size) -> Self {
        if size.width < 80 {
            Self::Compact
        } else if size.width < 120 {
            Self::Medium
        } else {
            Self::Wide
        }
    }
}

/// Common split result for two-region layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// First region.
    pub first: Rect,
    /// Second region.
    pub second: Rect,
}

/// Return an area inset by margins.
#[must_use]
pub const fn margin(area: Rect, insets: Insets) -> Rect {
    area.inset(insets)
}

/// Return a centered rectangle constrained by `size`.
#[must_use]
pub const fn centered(area: Rect, size: Size) -> Rect {
    let width = min_u16(area.width, size.width);
    let height = min_u16(area.height, size.height);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    Rect::new(x, y, width, height)
}

/// Split off a leading region by fixed length.
#[must_use]
pub const fn split_leading(area: Rect, direction: Direction, length: u16) -> Split {
    match direction {
        Direction::Horizontal => {
            let first_width = min_u16(area.width, length);
            Split {
                first: Rect::new(area.x, area.y, first_width, area.height),
                second: Rect::new(
                    area.x.saturating_add(first_width),
                    area.y,
                    area.width.saturating_sub(first_width),
                    area.height,
                ),
            }
        }
        Direction::Vertical => {
            let first_height = min_u16(area.height, length);
            Split {
                first: Rect::new(area.x, area.y, area.width, first_height),
                second: Rect::new(
                    area.x,
                    area.y.saturating_add(first_height),
                    area.width,
                    area.height.saturating_sub(first_height),
                ),
            }
        }
    }
}

/// Split off a trailing region by fixed length.
#[must_use]
pub const fn split_trailing(area: Rect, direction: Direction, length: u16) -> Split {
    match direction {
        Direction::Horizontal => {
            let second_width = min_u16(area.width, length);
            Split {
                first: Rect::new(
                    area.x,
                    area.y,
                    area.width.saturating_sub(second_width),
                    area.height,
                ),
                second: Rect::new(
                    area.right().saturating_sub(second_width),
                    area.y,
                    second_width,
                    area.height,
                ),
            }
        }
        Direction::Vertical => {
            let second_height = min_u16(area.height, length);
            Split {
                first: Rect::new(
                    area.x,
                    area.y,
                    area.width,
                    area.height.saturating_sub(second_height),
                ),
                second: Rect::new(
                    area.x,
                    area.bottom().saturating_sub(second_height),
                    area.width,
                    second_height,
                ),
            }
        }
    }
}

/// Rectangles produced by a dock layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DockAreas {
    /// Top docked area.
    pub top: Option<Rect>,
    /// Bottom docked area.
    pub bottom: Option<Rect>,
    /// Left docked area.
    pub left: Option<Rect>,
    /// Right docked area.
    pub right: Option<Rect>,
    /// Remaining center area.
    pub center: Rect,
}

/// Fixed-edge dock layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DockLayout {
    top: Option<u16>,
    bottom: Option<u16>,
    left: Option<u16>,
    right: Option<u16>,
}

impl DockLayout {
    /// Create an empty dock layout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            top: None,
            bottom: None,
            left: None,
            right: None,
        }
    }

    /// Dock a fixed-height top area.
    #[must_use]
    pub const fn top(mut self, height: u16) -> Self {
        self.top = Some(height);
        self
    }

    /// Dock a fixed-height bottom area.
    #[must_use]
    pub const fn bottom(mut self, height: u16) -> Self {
        self.bottom = Some(height);
        self
    }

    /// Dock a fixed-width left area.
    #[must_use]
    pub const fn left(mut self, width: u16) -> Self {
        self.left = Some(width);
        self
    }

    /// Dock a fixed-width right area.
    #[must_use]
    pub const fn right(mut self, width: u16) -> Self {
        self.right = Some(width);
        self
    }

    /// Resolve docked areas for `area`.
    #[must_use]
    pub const fn split(self, area: Rect) -> DockAreas {
        let mut center = area;
        let top = if let Some(height) = self.top {
            let split = split_leading(center, Direction::Vertical, height);
            center = split.second;
            Some(split.first)
        } else {
            None
        };
        let bottom = if let Some(height) = self.bottom {
            let split = split_trailing(center, Direction::Vertical, height);
            center = split.first;
            Some(split.second)
        } else {
            None
        };
        let left = if let Some(width) = self.left {
            let split = split_leading(center, Direction::Horizontal, width);
            center = split.second;
            Some(split.first)
        } else {
            None
        };
        let right = if let Some(width) = self.right {
            let split = split_trailing(center, Direction::Horizontal, width);
            center = split.first;
            Some(split.second)
        } else {
            None
        };
        DockAreas {
            top,
            bottom,
            left,
            right,
            center,
        }
    }
}

/// A simple directional layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    direction: Direction,
    constraints: Vec<Constraint>,
}

impl Layout {
    /// Create a layout with the supplied direction.
    #[must_use]
    pub const fn new(direction: Direction) -> Self {
        Self {
            direction,
            constraints: Vec::new(),
        }
    }

    /// Create a horizontal layout.
    #[must_use]
    pub const fn horizontal() -> Self {
        Self::new(Direction::Horizontal)
    }

    /// Create a vertical layout.
    #[must_use]
    pub const fn vertical() -> Self {
        Self::new(Direction::Vertical)
    }

    /// Set segment constraints.
    #[must_use]
    pub fn constraints(mut self, constraints: impl Into<Vec<Constraint>>) -> Self {
        self.constraints = constraints.into();
        self
    }

    /// Split an area according to this layout.
    #[must_use]
    pub fn split(&self, area: Rect) -> Vec<Rect> {
        split(area, self.direction, &self.constraints)
    }
}

/// Split an area by direction and constraints.
#[must_use]
pub fn split(area: Rect, direction: Direction, constraints: &[Constraint]) -> Vec<Rect> {
    if constraints.is_empty() {
        return Vec::new();
    }

    let axis = match direction {
        Direction::Horizontal => area.width,
        Direction::Vertical => area.height,
    };
    let lengths = resolve_lengths(axis, constraints);
    rects_for_lengths(area, direction, &lengths)
}

fn resolve_lengths(axis: u16, constraints: &[Constraint]) -> Vec<u16> {
    let mut lengths = vec![0; constraints.len()];
    let mut remaining = axis;
    let mut weighted_indices = Vec::new();
    let mut total_weight = 0_u16;

    for (index, constraint) in constraints.iter().copied().enumerate() {
        match constraint {
            Constraint::Length(length) => {
                let assigned = length.min(remaining);
                lengths[index] = assigned;
                remaining = remaining.saturating_sub(assigned);
            }
            Constraint::Percentage(percent) => {
                let assigned = percentage_len(axis, percent).min(remaining);
                lengths[index] = assigned;
                remaining = remaining.saturating_sub(assigned);
            }
            Constraint::Min(min) => {
                let assigned = min.min(remaining);
                lengths[index] = assigned;
                remaining = remaining.saturating_sub(assigned);
                weighted_indices.push(index);
                total_weight = total_weight.saturating_add(1);
            }
            Constraint::Ratio(weight) => {
                if weight > 0 {
                    weighted_indices.push(index);
                    total_weight = total_weight.saturating_add(weight);
                }
            }
            Constraint::Fill => {
                weighted_indices.push(index);
                total_weight = total_weight.saturating_add(1);
            }
        }
    }

    if remaining > 0 && total_weight > 0 {
        distribute_remaining(
            remaining,
            constraints,
            &weighted_indices,
            total_weight,
            &mut lengths,
        );
    }

    lengths
}

fn percentage_len(axis: u16, percent: u16) -> u16 {
    let clamped = percent.min(100);
    let value = u32::from(axis) * u32::from(clamped) / 100;
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn distribute_remaining(
    remaining: u16,
    constraints: &[Constraint],
    weighted_indices: &[usize],
    total_weight: u16,
    lengths: &mut [u16],
) {
    let mut assigned_total = 0_u16;
    for (position, index) in weighted_indices.iter().copied().enumerate() {
        let weight = constraint_weight(constraints[index]);
        let mut assigned = if position + 1 == weighted_indices.len() {
            remaining.saturating_sub(assigned_total)
        } else {
            let value = u32::from(remaining) * u32::from(weight) / u32::from(total_weight);
            u16::try_from(value).unwrap_or(u16::MAX)
        };
        assigned = assigned.min(remaining.saturating_sub(assigned_total));
        lengths[index] = lengths[index].saturating_add(assigned);
        assigned_total = assigned_total.saturating_add(assigned);
    }
}

const fn constraint_weight(constraint: Constraint) -> u16 {
    match constraint {
        Constraint::Ratio(weight) if weight > 0 => weight,
        Constraint::Min(_) | Constraint::Fill => 1,
        Constraint::Length(_) | Constraint::Percentage(_) | Constraint::Ratio(_) => 0,
    }
}

const fn min_u16(a: u16, b: u16) -> u16 {
    if a < b { a } else { b }
}

fn rects_for_lengths(area: Rect, direction: Direction, lengths: &[u16]) -> Vec<Rect> {
    let mut rects = Vec::with_capacity(lengths.len());
    let mut offset = 0_u16;
    for length in lengths {
        let rect = match direction {
            Direction::Horizontal => {
                Rect::new(area.x.saturating_add(offset), area.y, *length, area.height)
            }
            Direction::Vertical => {
                Rect::new(area.x, area.y.saturating_add(offset), area.width, *length)
            }
        };
        rects.push(rect);
        offset = offset.saturating_add(*length);
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::{
        Breakpoint, Breakpoints, Constraint, Direction, DockLayout, Layout, Responsive, Split,
        centered, split, split_leading, split_trailing,
    };
    use crate::geometry::Rect;

    #[test]
    fn custom_breakpoints_classify_widths() {
        let breakpoints = Breakpoints {
            compact_max_exclusive: 50,
            wide_min: 100,
        };

        assert_eq!(
            breakpoints.classify(crate::geometry::Size::new(49, 20)),
            Breakpoint::Compact
        );
        assert_eq!(
            breakpoints.classify(crate::geometry::Size::new(50, 20)),
            Breakpoint::Medium
        );
        assert_eq!(
            breakpoints.classify(crate::geometry::Size::new(100, 20)),
            Breakpoint::Wide
        );
    }

    #[test]
    fn responsive_values_select_by_size() {
        let columns = Responsive::new(1, 2, 3);

        assert_eq!(columns.for_size(crate::geometry::Size::new(40, 20)), 1);
        assert_eq!(columns.for_size(crate::geometry::Size::new(100, 20)), 2);
        assert_eq!(columns.for_size(crate::geometry::Size::new(140, 20)), 3);
    }

    #[test]
    fn breakpoint_classifies_widths() {
        assert_eq!(
            Breakpoint::for_size(crate::geometry::Size::new(79, 24)),
            Breakpoint::Compact
        );
        assert_eq!(
            Breakpoint::for_size(crate::geometry::Size::new(80, 24)),
            Breakpoint::Medium
        );
        assert_eq!(
            Breakpoint::for_size(crate::geometry::Size::new(120, 24)),
            Breakpoint::Wide
        );
    }

    #[test]
    fn centered_clamps_to_area() {
        assert_eq!(
            centered(Rect::new(0, 0, 10, 6), crate::geometry::Size::new(4, 2)),
            Rect::new(3, 2, 4, 2)
        );
        assert_eq!(
            centered(Rect::new(0, 0, 3, 2), crate::geometry::Size::new(8, 8)),
            Rect::new(0, 0, 3, 2)
        );
    }

    #[test]
    fn leading_and_trailing_splits_saturate() {
        let area = Rect::new(0, 0, 5, 3);

        assert_eq!(
            split_leading(area, Direction::Horizontal, 2),
            Split {
                first: Rect::new(0, 0, 2, 3),
                second: Rect::new(2, 0, 3, 3)
            }
        );
        assert_eq!(
            split_trailing(area, Direction::Vertical, 9),
            Split {
                first: Rect::new(0, 0, 5, 0),
                second: Rect::new(0, 0, 5, 3)
            }
        );
    }

    #[test]
    fn tiny_terminal_layouts_remain_deterministic() {
        let area = Rect::new(0, 0, 1, 1);
        let dock = DockLayout::new()
            .top(1)
            .bottom(1)
            .left(1)
            .right(1)
            .split(area);
        let splits = Layout::horizontal()
            .constraints(vec![
                Constraint::Length(1),
                Constraint::Fill,
                Constraint::Length(1),
            ])
            .split(area);

        assert_eq!(dock.top, Some(Rect::new(0, 0, 1, 1)));
        assert_eq!(dock.center, Rect::new(1, 1, 0, 0));
        assert_eq!(
            splits,
            vec![
                Rect::new(0, 0, 1, 1),
                Rect::new(1, 0, 0, 1),
                Rect::new(1, 0, 0, 1)
            ]
        );
    }

    #[test]
    fn dock_layout_reserves_edges_and_center() {
        let areas = DockLayout::new()
            .top(1)
            .bottom(2)
            .left(3)
            .right(4)
            .split(Rect::new(0, 0, 20, 10));

        assert_eq!(areas.top, Some(Rect::new(0, 0, 20, 1)));
        assert_eq!(areas.bottom, Some(Rect::new(0, 8, 20, 2)));
        assert_eq!(areas.left, Some(Rect::new(0, 1, 3, 7)));
        assert_eq!(areas.right, Some(Rect::new(16, 1, 4, 7)));
        assert_eq!(areas.center, Rect::new(3, 1, 13, 7));
    }

    #[test]
    fn dock_layout_saturates_when_edges_exceed_area() {
        let areas = DockLayout::new()
            .top(9)
            .bottom(9)
            .left(9)
            .split(Rect::new(0, 0, 4, 4));

        assert_eq!(areas.top, Some(Rect::new(0, 0, 4, 4)));
        assert_eq!(areas.bottom, Some(Rect::new(0, 4, 4, 0)));
        assert_eq!(areas.left, Some(Rect::new(0, 4, 4, 0)));
        assert_eq!(areas.center, Rect::new(4, 4, 0, 0));
    }

    #[test]
    fn horizontal_split_respects_length_and_fill() {
        let area = Rect::new(0, 0, 10, 3);
        let rects = split(
            area,
            Direction::Horizontal,
            &[Constraint::Length(3), Constraint::Fill],
        );

        assert_eq!(rects, vec![Rect::new(0, 0, 3, 3), Rect::new(3, 0, 7, 3)]);
    }

    #[test]
    fn vertical_split_distributes_ratio_space() {
        let area = Rect::new(0, 0, 4, 12);
        let rects = Layout::vertical()
            .constraints(vec![Constraint::Ratio(1), Constraint::Ratio(2)])
            .split(area);

        assert_eq!(rects, vec![Rect::new(0, 0, 4, 4), Rect::new(0, 4, 4, 8)]);
    }

    #[test]
    fn split_saturates_when_fixed_lengths_exceed_area() {
        let area = Rect::new(0, 0, 5, 1);
        let rects = split(
            area,
            Direction::Horizontal,
            &[
                Constraint::Length(4),
                Constraint::Length(4),
                Constraint::Fill,
            ],
        );

        assert_eq!(
            rects,
            vec![
                Rect::new(0, 0, 4, 1),
                Rect::new(4, 0, 1, 1),
                Rect::new(5, 0, 0, 1)
            ]
        );
    }

    #[test]
    fn percentage_constraints_are_based_on_original_axis() {
        let area = Rect::new(0, 0, 10, 1);
        let rects = split(
            area,
            Direction::Horizontal,
            &[Constraint::Percentage(50), Constraint::Fill],
        );

        assert_eq!(rects, vec![Rect::new(0, 0, 5, 1), Rect::new(5, 0, 5, 1)]);
    }
}
