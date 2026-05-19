//! Deterministic terminal layout helpers.

use crate::geometry::Rect;

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
    use super::{Constraint, Direction, Layout, split};
    use crate::geometry::Rect;

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
